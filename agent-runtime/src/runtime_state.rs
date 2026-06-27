use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use redis::streams::StreamRangeReply;
use redis::Commands;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::event_protocol::{RuntimeCommand, RuntimeEvent};

const ACTIVE_STATE_TTL_SECONDS: u64 = 24 * 60 * 60;
const FINAL_STATE_TTL_SECONDS: u64 = 60 * 60;
const RUN_LOCK_TTL_SECONDS: u64 = 2 * 60 * 60;
const EVENT_STREAM_TTL_SECONDS: i64 = 24 * 60 * 60;
const CANCEL_SIGNAL_TTL_SECONDS: u64 = 10 * 60;
const APPROVAL_INDEX_TTL_SECONDS: u64 = 24 * 60 * 60;
const EVENT_STREAM_MAX_LEN: usize = 1000;
const DEFAULT_REPLAY_LIMIT: usize = 100;
const MAX_REPLAY_LIMIT: usize = 1000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RuntimeRunState {
    pub conversation_id: String,
    pub runtime_session_id: String,
    pub turn_id: String,
    pub status: String,
    pub message: String,
    pub assistant_message_id: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredRuntimeEvent {
    pub event_id: String,
    pub event: RuntimeEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CancelSignal {
    pub reason: String,
    pub continue_after: bool,
    pub requested_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ApprovalIndex {
    pub request_id: String,
    pub conversation_id: String,
    pub runtime_session_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub assistant_message_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RunLock {
    conversation_id: String,
    runtime_session_id: String,
    turn_id: String,
    owner: String,
    started_at: String,
    heartbeat_at: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeStateStore {
    inner: Arc<RuntimeStateInner>,
}

#[derive(Debug)]
struct RuntimeStateInner {
    memory: Mutex<MemoryState>,
    redis: Option<RedisState>,
}

#[derive(Debug, Default)]
struct MemoryState {
    states: HashMap<String, RuntimeRunState>,
    locks: HashSet<String>,
    events: HashMap<String, Vec<StoredRuntimeEvent>>,
    cancels: HashMap<String, CancelSignal>,
    approvals: HashMap<String, ApprovalIndex>,
    next_event_id: u64,
}

#[derive(Debug, Clone)]
struct RedisState {
    client: redis::Client,
    prefix: String,
}

#[derive(Debug)]
pub struct RuntimeRunGuard {
    store: RuntimeStateStore,
    conversation_id: String,
    owner: String,
    redis_claimed: bool,
    memory_claimed: bool,
}

impl RuntimeRunGuard {
    pub fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }
}

impl RuntimeStateStore {
    pub fn new(redis_addr: Option<String>, redis_prefix: String) -> Self {
        let redis = redis_addr.and_then(|addr| RedisState::connect(addr, redis_prefix));
        Self {
            inner: Arc::new(RuntimeStateInner {
                memory: Mutex::new(MemoryState::default()),
                redis,
            }),
        }
    }

    pub fn new_required(redis_addr: Option<String>, redis_prefix: String) -> Result<Self, String> {
        let Some(addr) = redis_addr.filter(|addr| !addr.trim().is_empty()) else {
            return Err("agent runtime grpc requires redis_addr".to_string());
        };
        let Some(redis) = RedisState::connect(addr.clone(), redis_prefix) else {
            return Err(format!("connect agent runtime redis {addr}"));
        };
        Ok(Self {
            inner: Arc::new(RuntimeStateInner {
                memory: Mutex::new(MemoryState::default()),
                redis: Some(redis),
            }),
        })
    }

    pub fn claim_run(&self, conversation_id: &str) -> Result<Option<RuntimeRunGuard>, String> {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return Ok(None);
        }
        let owner = Uuid::new_v4().simple().to_string();
        let mut redis_claimed = false;
        if let Some(redis) = &self.inner.redis {
            redis_claimed = redis.claim_run(conversation_id, &owner)?;
            if !redis_claimed {
                return Err("conversation already has an active runtime submission".to_string());
            }
        }

        let mut memory = self.inner.memory.lock().expect("runtime state poisoned");
        if !memory.locks.insert(conversation_id.to_string()) {
            if redis_claimed {
                if let Some(redis) = &self.inner.redis {
                    redis.release_run(conversation_id, &owner);
                }
            }
            return Err("conversation already has an active runtime submission".to_string());
        }
        drop(memory);

        Ok(Some(RuntimeRunGuard {
            store: self.clone(),
            conversation_id: conversation_id.to_string(),
            owner,
            redis_claimed,
            memory_claimed: true,
        }))
    }

    pub fn heartbeat_run(&self, conversation_id: &str, owner: &str) {
        if conversation_id.trim().is_empty() || owner.trim().is_empty() {
            return;
        }
        if let Some(redis) = &self.inner.redis {
            redis.heartbeat_run(conversation_id, owner);
        }
    }

    pub fn request_cancel(
        &self,
        conversation_id: &str,
        reason: &str,
        continue_after: bool,
    ) -> bool {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return false;
        }
        let signal = CancelSignal {
            reason: reason.trim().to_string(),
            continue_after,
            requested_at: now_unix_string(),
        };
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .cancels
            .insert(conversation_id.to_string(), signal.clone());
        if let Some(redis) = &self.inner.redis {
            redis.set_cancel_signal(conversation_id, &signal);
        }
        true
    }

    pub fn cancel_signal(&self, conversation_id: &str) -> Option<CancelSignal> {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return None;
        }
        if let Some(redis) = &self.inner.redis {
            if let Some(signal) = redis.get_cancel_signal(conversation_id) {
                return Some(signal);
            }
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .cancels
            .get(conversation_id)
            .cloned()
    }

    pub fn clear_cancel_signal(&self, conversation_id: &str) {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return;
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .cancels
            .remove(conversation_id);
        if let Some(redis) = &self.inner.redis {
            redis.clear_cancel_signal(conversation_id);
        }
    }

    pub fn resolve_approval(&self, request_id: &str) -> Option<ApprovalIndex> {
        let request_id = request_id.trim();
        if request_id.is_empty() {
            return None;
        }
        if let Some(redis) = &self.inner.redis {
            if let Some(index) = redis.get_approval_index(request_id) {
                return Some(index);
            }
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .approvals
            .get(request_id)
            .cloned()
    }

    pub fn mark_command_started(&self, command: &RuntimeCommand) {
        let Some((conversation_id, runtime_session_id, message, assistant_message_id)) =
            command_state_seed(command)
        else {
            return;
        };
        self.upsert(RuntimeRunState {
            conversation_id,
            runtime_session_id,
            turn_id: String::new(),
            status: "running".to_string(),
            message,
            assistant_message_id,
            updated_at: now_unix_string(),
        });
    }

    pub fn apply_event(&self, event: &RuntimeEvent) {
        let Some((conversation_id, runtime_session_id, turn_id, status, message)) =
            event_state_update(event)
        else {
            self.apply_event_indexes(event);
            return;
        };
        let mut state = self
            .get(&conversation_id)
            .unwrap_or_else(|| RuntimeRunState {
                conversation_id: conversation_id.clone(),
                ..RuntimeRunState::default()
            });
        if !runtime_session_id.is_empty() {
            state.runtime_session_id = runtime_session_id.clone();
        }
        if !turn_id.is_empty() {
            state.turn_id = turn_id.clone();
        }
        if !status.is_empty() {
            state.status = status;
        }
        if !message.is_empty() {
            state.message = message;
        }
        state.updated_at = now_unix_string();
        self.upsert(state);
        if !runtime_session_id.is_empty() || !turn_id.is_empty() {
            if let Some(redis) = &self.inner.redis {
                redis.update_run_lock_state(
                    &conversation_id,
                    runtime_session_id.as_str(),
                    turn_id.as_str(),
                );
            }
        }
        self.apply_event_indexes(event);
    }

    pub fn append_event(&self, event: &RuntimeEvent) -> Option<String> {
        let conversation_id = event_conversation_id(event);
        if conversation_id.trim().is_empty() {
            return None;
        }
        if let Some(redis) = &self.inner.redis {
            if let Some(event_id) = redis.append_event(event, &conversation_id) {
                return Some(event_id);
            }
        }

        let mut memory = self.inner.memory.lock().expect("runtime state poisoned");
        memory.next_event_id += 1;
        let event_id = memory.next_event_id.to_string();
        let events = memory.events.entry(conversation_id).or_default();
        events.push(StoredRuntimeEvent {
            event_id: event_id.clone(),
            event: event.clone(),
        });
        if events.len() > EVENT_STREAM_MAX_LEN {
            let remove_count = events.len() - EVENT_STREAM_MAX_LEN;
            events.drain(0..remove_count);
        }
        Some(event_id)
    }

    pub fn list_events(
        &self,
        conversation_id: &str,
        after_event_id: Option<&str>,
        limit: usize,
    ) -> Vec<StoredRuntimeEvent> {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return Vec::new();
        }
        let limit = normalize_replay_limit(limit);
        if let Some(redis) = &self.inner.redis {
            let events = redis.list_events(conversation_id, after_event_id, limit);
            if !events.is_empty() {
                return events;
            }
        }
        let memory = self.inner.memory.lock().expect("runtime state poisoned");
        let Some(events) = memory.events.get(conversation_id) else {
            return Vec::new();
        };
        let start = match after_event_id.filter(|id| !id.trim().is_empty()) {
            Some(after) => events
                .iter()
                .position(|stored| stored.event_id == after)
                .map(|idx| idx + 1)
                .unwrap_or(0),
            None => 0,
        };
        events.iter().skip(start).take(limit).cloned().collect()
    }

    pub fn mark_status(&self, conversation_id: &str, status: &str, message: &str) {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return;
        }
        let mut state = self
            .get(conversation_id)
            .unwrap_or_else(|| RuntimeRunState {
                conversation_id: conversation_id.to_string(),
                ..RuntimeRunState::default()
            });
        state.status = status.to_string();
        state.message = message.to_string();
        state.updated_at = now_unix_string();
        self.upsert(state);
    }

    pub fn get(&self, conversation_id: &str) -> Option<RuntimeRunState> {
        let conversation_id = conversation_id.trim();
        if conversation_id.is_empty() {
            return None;
        }
        if let Some(redis) = &self.inner.redis {
            if let Some(state) = redis.get_state(conversation_id) {
                return Some(state);
            }
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .states
            .get(conversation_id)
            .cloned()
    }

    pub fn list_active(&self) -> Vec<RuntimeRunState> {
        if let Some(redis) = &self.inner.redis {
            let states = redis.list_states();
            if !states.is_empty() {
                return states
                    .into_iter()
                    .filter(|state| is_active_status(&state.status))
                    .collect();
            }
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .states
            .values()
            .filter(|state| is_active_status(&state.status))
            .cloned()
            .collect()
    }

    fn upsert(&self, state: RuntimeRunState) {
        if state.conversation_id.trim().is_empty() {
            return;
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .states
            .insert(state.conversation_id.clone(), state.clone());
        if let Some(redis) = &self.inner.redis {
            redis.set_state(&state);
        }
    }

    fn apply_event_indexes(&self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::ApprovalRequested {
                conversation_id,
                runtime_session_id,
                turn_id,
                request_id,
                tool_call_id,
                tool_name,
                ..
            } => {
                let index = ApprovalIndex {
                    request_id: request_id.clone(),
                    conversation_id: conversation_id.clone(),
                    runtime_session_id: runtime_session_id.clone(),
                    turn_id: turn_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    assistant_message_id: String::new(),
                    created_at: now_unix_string(),
                };
                self.inner
                    .memory
                    .lock()
                    .expect("runtime state poisoned")
                    .approvals
                    .insert(request_id.clone(), index.clone());
                if let Some(redis) = &self.inner.redis {
                    redis.set_approval_index(&index);
                }
            }
            RuntimeEvent::ApprovalResolved { request_id, .. } => {
                self.clear_approval_index(request_id);
            }
            _ => {}
        }
        if let Some(conversation_id) = terminal_conversation_id(event) {
            self.clear_cancel_signal(&conversation_id);
            if let Some(redis) = &self.inner.redis {
                redis.expire_event_stream(&conversation_id);
            }
        }
    }

    fn clear_approval_index(&self, request_id: &str) {
        let request_id = request_id.trim();
        if request_id.is_empty() {
            return;
        }
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .approvals
            .remove(request_id);
        if let Some(redis) = &self.inner.redis {
            redis.clear_approval_index(request_id);
        }
    }

    fn release_memory_run(&self, conversation_id: &str) {
        self.inner
            .memory
            .lock()
            .expect("runtime state poisoned")
            .locks
            .remove(conversation_id);
    }
}

impl Drop for RuntimeRunGuard {
    fn drop(&mut self) {
        self.store.clear_cancel_signal(&self.conversation_id);
        if self.memory_claimed {
            self.store.release_memory_run(&self.conversation_id);
            self.memory_claimed = false;
        }
        if self.redis_claimed {
            if let Some(redis) = &self.store.inner.redis {
                redis.release_run(&self.conversation_id, &self.owner);
            }
            self.redis_claimed = false;
        }
    }
}

impl RedisState {
    fn connect(addr: String, prefix: String) -> Option<Self> {
        let url = if addr.contains("://") {
            addr
        } else {
            format!("redis://{addr}/")
        };
        let client = redis::Client::open(url).ok()?;
        let mut conn = client.get_connection().ok()?;
        let pong: redis::RedisResult<String> = redis::cmd("PING").query(&mut conn);
        if pong.is_err() {
            return None;
        }
        Some(Self { client, prefix })
    }

    fn state_key(&self, conversation_id: &str) -> String {
        format!("{}state:{}", self.prefix, conversation_id)
    }

    fn run_lock_key(&self, conversation_id: &str) -> String {
        format!("{}run_lock:{}", self.prefix, conversation_id)
    }

    fn active_states_key(&self) -> String {
        format!("{}active_states", self.prefix)
    }

    fn cancel_key(&self, conversation_id: &str) -> String {
        format!("{}cancel:{}", self.prefix, conversation_id)
    }

    fn approval_key(&self, request_id: &str) -> String {
        format!("{}approval:{}", self.prefix, request_id)
    }

    fn event_stream_key(&self, conversation_id: &str) -> String {
        format!("{}events:{}", self.prefix, conversation_id)
    }

    fn claim_run(&self, conversation_id: &str, owner: &str) -> Result<bool, String> {
        let mut conn = self
            .client
            .get_connection()
            .map_err(|err| err.to_string())?;
        let now = now_unix_string();
        let lock = RunLock {
            conversation_id: conversation_id.to_string(),
            owner: owner.to_string(),
            started_at: now.clone(),
            heartbeat_at: now,
            ..RunLock::default()
        };
        let raw = serde_json::to_string(&lock).map_err(|err| err.to_string())?;
        let result: Option<String> = redis::cmd("SET")
            .arg(self.run_lock_key(conversation_id))
            .arg(raw)
            .arg("NX")
            .arg("EX")
            .arg(RUN_LOCK_TTL_SECONDS)
            .query(&mut conn)
            .map_err(|err| err.to_string())?;
        Ok(result.is_some())
    }

    fn release_run(&self, conversation_id: &str, owner: &str) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let key = self.run_lock_key(conversation_id);
        let current: redis::RedisResult<Option<String>> = conn.get(&key);
        if lock_owner_matches(current.ok().flatten().as_deref(), owner) {
            let _: redis::RedisResult<()> = conn.del(key);
        }
    }

    fn heartbeat_run(&self, conversation_id: &str, owner: &str) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let key = self.run_lock_key(conversation_id);
        let raw: redis::RedisResult<Option<String>> = conn.get(&key);
        let Some(raw) = raw.ok().flatten() else {
            return;
        };
        let Some(mut lock) = parse_run_lock(&raw) else {
            return;
        };
        if lock.owner != owner {
            return;
        }
        lock.heartbeat_at = now_unix_string();
        let Ok(updated) = serde_json::to_string(&lock) else {
            return;
        };
        let _: redis::RedisResult<()> = conn.set_ex(key, updated, RUN_LOCK_TTL_SECONDS);
    }

    fn update_run_lock_state(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
        turn_id: &str,
    ) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let key = self.run_lock_key(conversation_id);
        let raw: redis::RedisResult<Option<String>> = conn.get(&key);
        let Some(raw) = raw.ok().flatten() else {
            return;
        };
        let Some(mut lock) = parse_run_lock(&raw) else {
            return;
        };
        if !runtime_session_id.trim().is_empty() {
            lock.runtime_session_id = runtime_session_id.to_string();
        }
        if !turn_id.trim().is_empty() {
            lock.turn_id = turn_id.to_string();
        }
        lock.heartbeat_at = now_unix_string();
        let Ok(updated) = serde_json::to_string(&lock) else {
            return;
        };
        let _: redis::RedisResult<()> = conn.set_ex(key, updated, RUN_LOCK_TTL_SECONDS);
    }

    fn set_state(&self, state: &RuntimeRunState) {
        let Ok(raw) = serde_json::to_string(state) else {
            return;
        };
        let ttl = if is_active_status(&state.status) {
            ACTIVE_STATE_TTL_SECONDS
        } else {
            FINAL_STATE_TTL_SECONDS
        };
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<()> =
            conn.set_ex(self.state_key(&state.conversation_id), raw, ttl);
        if is_active_status(&state.status) {
            let _: redis::RedisResult<()> = redis::cmd("SADD")
                .arg(self.active_states_key())
                .arg(&state.conversation_id)
                .query(&mut conn);
        } else {
            let _: redis::RedisResult<()> = redis::cmd("SREM")
                .arg(self.active_states_key())
                .arg(&state.conversation_id)
                .query(&mut conn);
        }
    }

    fn get_state(&self, conversation_id: &str) -> Option<RuntimeRunState> {
        let mut conn = self.client.get_connection().ok()?;
        let raw: Option<String> = conn.get(self.state_key(conversation_id)).ok()?;
        serde_json::from_str(&raw?).ok()
    }

    fn list_states(&self) -> Vec<RuntimeRunState> {
        let Ok(mut conn) = self.client.get_connection() else {
            return Vec::new();
        };
        let conversations: Vec<String> = redis::cmd("SMEMBERS")
            .arg(self.active_states_key())
            .query(&mut conn)
            .unwrap_or_default();
        let mut states = Vec::new();
        for conversation_id in conversations {
            let raw: redis::RedisResult<Option<String>> =
                conn.get(self.state_key(&conversation_id));
            if let Some(raw) = raw.ok().flatten() {
                if let Ok(state) = serde_json::from_str::<RuntimeRunState>(&raw) {
                    if is_active_status(&state.status) {
                        states.push(state);
                    } else {
                        let _: redis::RedisResult<()> = redis::cmd("SREM")
                            .arg(self.active_states_key())
                            .arg(conversation_id)
                            .query(&mut conn);
                    }
                }
            } else {
                let _: redis::RedisResult<()> = redis::cmd("SREM")
                    .arg(self.active_states_key())
                    .arg(conversation_id)
                    .query(&mut conn);
            }
        }
        if states.is_empty() {
            states = self.scan_states(&mut conn);
        }
        states
    }

    fn scan_states(&self, conn: &mut redis::Connection) -> Vec<RuntimeRunState> {
        let mut cursor = 0_u64;
        let mut states = Vec::new();
        loop {
            let reply: redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(format!("{}state:*", self.prefix))
                .arg("COUNT")
                .arg(100)
                .query(&mut *conn);
            let Ok((next, keys)) = reply else {
                break;
            };
            for key in keys {
                let raw: redis::RedisResult<Option<String>> = conn.get(key);
                if let Some(raw) = raw.ok().flatten() {
                    if let Ok(state) = serde_json::from_str::<RuntimeRunState>(&raw) {
                        if is_active_status(&state.status) {
                            let _: redis::RedisResult<()> = redis::cmd("SADD")
                                .arg(self.active_states_key())
                                .arg(&state.conversation_id)
                                .query(&mut *conn);
                            states.push(state);
                        }
                    }
                }
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
        states
    }

    fn set_cancel_signal(&self, conversation_id: &str, signal: &CancelSignal) {
        let Ok(raw) = serde_json::to_string(signal) else {
            return;
        };
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<()> = conn.set_ex(
            self.cancel_key(conversation_id),
            raw,
            CANCEL_SIGNAL_TTL_SECONDS,
        );
    }

    fn get_cancel_signal(&self, conversation_id: &str) -> Option<CancelSignal> {
        let mut conn = self.client.get_connection().ok()?;
        let raw: Option<String> = conn.get(self.cancel_key(conversation_id)).ok()?;
        serde_json::from_str(&raw?).ok()
    }

    fn clear_cancel_signal(&self, conversation_id: &str) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<()> = conn.del(self.cancel_key(conversation_id));
    }

    fn set_approval_index(&self, index: &ApprovalIndex) {
        let Ok(raw) = serde_json::to_string(index) else {
            return;
        };
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<()> = conn.set_ex(
            self.approval_key(&index.request_id),
            raw,
            APPROVAL_INDEX_TTL_SECONDS,
        );
    }

    fn get_approval_index(&self, request_id: &str) -> Option<ApprovalIndex> {
        let mut conn = self.client.get_connection().ok()?;
        let raw: Option<String> = conn.get(self.approval_key(request_id)).ok()?;
        serde_json::from_str(&raw?).ok()
    }

    fn clear_approval_index(&self, request_id: &str) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<()> = conn.del(self.approval_key(request_id));
    }

    fn expire_event_stream(&self, conversation_id: &str) {
        let Ok(mut conn) = self.client.get_connection() else {
            return;
        };
        let _: redis::RedisResult<bool> = conn.expire(
            self.event_stream_key(conversation_id),
            EVENT_STREAM_TTL_SECONDS,
        );
    }

    fn append_event(&self, event: &RuntimeEvent, conversation_id: &str) -> Option<String> {
        let raw = serde_json::to_string(event).ok()?;
        let value = serde_json::to_value(event).ok()?;
        let event_type = value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let created_at = now_unix_string();
        let payload_json = serde_json::to_string(&value).unwrap_or_default();
        let runtime_trace_json = runtime_trace_json(&value, event_type).unwrap_or_default();
        let mut conn = self.client.get_connection().ok()?;
        let event_id: String = redis::cmd("XADD")
            .arg(self.event_stream_key(conversation_id))
            .arg("MAXLEN")
            .arg("~")
            .arg(EVENT_STREAM_MAX_LEN)
            .arg("*")
            .arg("raw_json")
            .arg(raw)
            .arg("type")
            .arg(event_type)
            .arg("runtime_event_type")
            .arg(event_type)
            .arg("runtime_trace_json")
            .arg(runtime_trace_json)
            .arg("payload_json")
            .arg(payload_json)
            .arg("conversation_id")
            .arg(conversation_id)
            .arg("runtime_session_id")
            .arg(
                value
                    .get("runtime_session_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            )
            .arg("turn_id")
            .arg(
                value
                    .get("turn_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            )
            .arg("command_id")
            .arg(
                value
                    .get("command_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default(),
            )
            .arg("created_at_unix")
            .arg(created_at)
            .query(&mut conn)
            .ok()?;
        Some(event_id)
    }

    fn list_events(
        &self,
        conversation_id: &str,
        after_event_id: Option<&str>,
        limit: usize,
    ) -> Vec<StoredRuntimeEvent> {
        let Ok(mut conn) = self.client.get_connection() else {
            return Vec::new();
        };
        let start = after_event_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(|id| format!("({id}"))
            .unwrap_or_else(|| "-".to_string());
        let raw_events: StreamRangeReply = match redis::cmd("XRANGE")
            .arg(self.event_stream_key(conversation_id))
            .arg(start)
            .arg("+")
            .arg("COUNT")
            .arg(limit)
            .query(&mut conn)
        {
            Ok(reply) => reply,
            Err(_) => return Vec::new(),
        };
        raw_events
            .ids
            .into_iter()
            .filter_map(|stream_id| {
                let raw_json = stream_id
                    .map
                    .get("raw_json")
                    .and_then(|value| redis::from_redis_value::<String>(value).ok())?;
                let event = serde_json::from_str(&raw_json).ok()?;
                Some(StoredRuntimeEvent {
                    event_id: stream_id.id,
                    event,
                })
            })
            .collect()
    }
}

fn runtime_trace_json(value: &serde_json::Value, event_type: &str) -> Option<String> {
    let mut trace = serde_json::Map::new();
    trace.insert(
        "schema".to_string(),
        serde_json::Value::String("cyberstrike.agent_runtime.trace.v1".to_string()),
    );
    trace.insert(
        "event".to_string(),
        serde_json::Value::String(event_type.to_string()),
    );
    copy_trace_string(value, &mut trace, "conversation_id", "conversationId");
    copy_trace_string(value, &mut trace, "runtime_session_id", "runtimeSessionId");
    copy_trace_string(value, &mut trace, "turn_id", "turnId");
    copy_trace_string(value, &mut trace, "message", "message");
    copy_trace_string(value, &mut trace, "delta", "delta");
    copy_trace_string(value, &mut trace, "accumulated", "accumulated");
    copy_trace_string(value, &mut trace, "response", "response");
    copy_trace_string(value, &mut trace, "reason", "reason");
    copy_trace_string(value, &mut trace, "summary", "summary");
    if let Some(items) = value.get("items") {
        trace.insert("items".to_string(), items.clone());
        trace.insert("plan".to_string(), items.clone());
    }
    if value.get("tool_call_id").is_some()
        || value.get("tool_name").is_some()
        || value.get("arguments").is_some()
        || value.get("result").is_some()
        || value.get("error").is_some()
    {
        let mut tool = serde_json::Map::new();
        copy_trace_string(value, &mut tool, "tool_call_id", "callId");
        copy_trace_string(value, &mut tool, "tool_name", "name");
        if let Some(arguments) = value.get("arguments") {
            tool.insert("arguments".to_string(), arguments.clone());
        }
        copy_trace_string(value, &mut tool, "result", "result");
        copy_trace_string(value, &mut tool, "error", "error");
        trace.insert("tool".to_string(), serde_json::Value::Object(tool));
    }
    serde_json::to_string(&serde_json::Value::Object(trace)).ok()
}

fn copy_trace_string(
    from: &serde_json::Value,
    to: &mut serde_json::Map<String, serde_json::Value>,
    source_key: &str,
    target_key: &str,
) {
    if let Some(value) = from.get(source_key).and_then(|v| v.as_str()) {
        if !value.trim().is_empty() {
            to.insert(
                target_key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
        }
    }
}

fn parse_run_lock(raw: &str) -> Option<RunLock> {
    serde_json::from_str(raw).ok()
}

fn lock_owner_matches(raw: Option<&str>, owner: &str) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    if raw == owner {
        return true;
    }
    parse_run_lock(raw)
        .map(|lock| lock.owner == owner)
        .unwrap_or(false)
}

fn command_state_seed(command: &RuntimeCommand) -> Option<(String, String, String, String)> {
    match command {
        RuntimeCommand::StartTurn {
            conversation_id,
            runtime_session_id,
            message,
            context,
            ..
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone().unwrap_or_default(),
            message.clone(),
            assistant_message_id_from_context(context),
        )),
        RuntimeCommand::ApprovalResponse {
            conversation_id,
            runtime_session_id,
            request_id,
            context,
            ..
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone().unwrap_or_default(),
            format!("resuming approval {request_id}"),
            assistant_message_id_from_context(context),
        )),
        RuntimeCommand::InterruptTurn {
            conversation_id,
            reason,
            ..
        } => Some((
            conversation_id.clone(),
            String::new(),
            reason.clone(),
            String::new(),
        )),
        RuntimeCommand::Shutdown => None,
    }
}

fn assistant_message_id_from_context(
    context: &serde_json::Map<String, serde_json::Value>,
) -> String {
    context
        .get("assistant_message_id")
        .or_else(|| context.get("assistantMessageId"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn event_state_update(event: &RuntimeEvent) -> Option<(String, String, String, String, String)> {
    match event {
        RuntimeEvent::SessionStarted {
            conversation_id,
            runtime_session_id,
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            String::new(),
            "running".to_string(),
            "session started".to_string(),
        )),
        RuntimeEvent::TurnStarted {
            conversation_id,
            runtime_session_id,
            turn_id,
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            turn_id.clone(),
            "running".to_string(),
            "turn started".to_string(),
        )),
        RuntimeEvent::RuntimeStatusUpdate {
            conversation_id,
            runtime_session_id,
            turn_id,
            message,
        }
        | RuntimeEvent::AssistantProgressUpdate {
            conversation_id,
            runtime_session_id,
            turn_id,
            message,
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            turn_id.clone(),
            "running".to_string(),
            message.clone(),
        )),
        RuntimeEvent::ApprovalRequested {
            conversation_id,
            runtime_session_id,
            turn_id,
            message,
            ..
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            turn_id.clone(),
            "awaiting_approval".to_string(),
            message.clone(),
        )),
        RuntimeEvent::TurnCompleted {
            conversation_id,
            runtime_session_id,
            turn_id,
            ..
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            turn_id.clone(),
            "completed".to_string(),
            "turn completed".to_string(),
        )),
        RuntimeEvent::TurnAborted {
            conversation_id,
            runtime_session_id,
            turn_id,
            reason,
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            turn_id.clone(),
            "cancelled".to_string(),
            reason.clone(),
        )),
        RuntimeEvent::RuntimeError {
            conversation_id,
            runtime_session_id,
            message,
        } => Some((
            conversation_id.clone(),
            runtime_session_id.clone(),
            String::new(),
            "failed".to_string(),
            message.clone(),
        )),
        _ => None,
    }
}

fn is_active_status(status: &str) -> bool {
    matches!(status, "running" | "awaiting_approval" | "cancelling")
}

fn event_conversation_id(event: &RuntimeEvent) -> String {
    match event {
        RuntimeEvent::SessionStarted {
            conversation_id, ..
        }
        | RuntimeEvent::TurnStarted {
            conversation_id, ..
        }
        | RuntimeEvent::PlanUpdated {
            conversation_id, ..
        }
        | RuntimeEvent::ReasoningDelta {
            conversation_id, ..
        }
        | RuntimeEvent::AssistantProgressUpdate {
            conversation_id, ..
        }
        | RuntimeEvent::RuntimeStatusUpdate {
            conversation_id, ..
        }
        | RuntimeEvent::AssistantDelta {
            conversation_id, ..
        }
        | RuntimeEvent::ToolCallStarted {
            conversation_id, ..
        }
        | RuntimeEvent::ToolCallDelta {
            conversation_id, ..
        }
        | RuntimeEvent::ToolCallCompleted {
            conversation_id, ..
        }
        | RuntimeEvent::ToolCallFailed {
            conversation_id, ..
        }
        | RuntimeEvent::ApprovalRequested {
            conversation_id, ..
        }
        | RuntimeEvent::ApprovalResolved {
            conversation_id, ..
        }
        | RuntimeEvent::FollowUpStarted {
            conversation_id, ..
        }
        | RuntimeEvent::CompactionStarted {
            conversation_id, ..
        }
        | RuntimeEvent::CompactionCompleted {
            conversation_id, ..
        }
        | RuntimeEvent::StopHookContinued {
            conversation_id, ..
        }
        | RuntimeEvent::TurnCompleted {
            conversation_id, ..
        }
        | RuntimeEvent::TurnAborted {
            conversation_id, ..
        }
        | RuntimeEvent::RuntimeError {
            conversation_id, ..
        }
        | RuntimeEvent::CommandCompleted {
            conversation_id, ..
        } => conversation_id.clone(),
    }
}

fn terminal_conversation_id(event: &RuntimeEvent) -> Option<String> {
    match event {
        RuntimeEvent::TurnCompleted {
            conversation_id, ..
        }
        | RuntimeEvent::TurnAborted {
            conversation_id, ..
        }
        | RuntimeEvent::RuntimeError {
            conversation_id, ..
        }
        | RuntimeEvent::CommandCompleted {
            conversation_id, ..
        } if !conversation_id.trim().is_empty() => Some(conversation_id.clone()),
        _ => None,
    }
}

fn normalize_replay_limit(limit: usize) -> usize {
    match limit {
        0 => DEFAULT_REPLAY_LIMIT,
        n if n > MAX_REPLAY_LIMIT => MAX_REPLAY_LIMIT,
        n => n,
    }
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_tracks_active_and_terminal_state() {
        let store = RuntimeStateStore::new(None, "test:".to_string());
        store.mark_command_started(&RuntimeCommand::StartTurn {
            command_id: "cmd-1".to_string(),
            conversation_id: "conv-1".to_string(),
            runtime_session_id: None,
            message: "hello".to_string(),
            context: serde_json::Map::new(),
        });
        assert_eq!(store.list_active().len(), 1);
        store.apply_event(&RuntimeEvent::ApprovalRequested {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            request_id: "req-1".to_string(),
            permission: "tool".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "runtime_echo".to_string(),
            arguments: serde_json::Value::Null,
            message: "approval needed".to_string(),
        });
        let state = store.get("conv-1").expect("state");
        assert_eq!(state.status, "awaiting_approval");
        assert_eq!(state.runtime_session_id, "session-1");
        store.apply_event(&RuntimeEvent::TurnCompleted {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            response: "done".to_string(),
        });
        assert!(store.list_active().is_empty());
    }

    #[test]
    fn memory_store_rejects_duplicate_run_claims() {
        let store = RuntimeStateStore::new(None, "test:".to_string());
        let _guard = store.claim_run("conv-1").expect("claim").expect("guard");
        assert!(store.claim_run("conv-1").is_err());
    }

    #[test]
    fn memory_store_replays_events_after_cursor() {
        let store = RuntimeStateStore::new(None, "test:".to_string());
        let first = store
            .append_event(&RuntimeEvent::TurnStarted {
                conversation_id: "conv-1".to_string(),
                runtime_session_id: "session-1".to_string(),
                turn_id: "turn-1".to_string(),
            })
            .expect("first event id");
        let second = store
            .append_event(&RuntimeEvent::AssistantDelta {
                conversation_id: "conv-1".to_string(),
                runtime_session_id: "session-1".to_string(),
                turn_id: "turn-1".to_string(),
                delta: "hi".to_string(),
                accumulated: "hi".to_string(),
            })
            .expect("second event id");

        let replay = store.list_events("conv-1", Some(&first), 10);
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].event_id, second);
        assert!(matches!(
            replay[0].event,
            RuntimeEvent::AssistantDelta { .. }
        ));
    }

    #[test]
    fn memory_store_tracks_cancel_signal_until_terminal_event() {
        let store = RuntimeStateStore::new(None, "test:".to_string());
        assert!(store.request_cancel("conv-1", "stop now", false));
        let signal = store.cancel_signal("conv-1").expect("cancel signal");
        assert_eq!(signal.reason, "stop now");
        assert!(!signal.continue_after);

        store.apply_event(&RuntimeEvent::TurnAborted {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            reason: "stop now".to_string(),
        });

        assert!(store.cancel_signal("conv-1").is_none());
    }

    #[test]
    fn memory_store_resolves_and_clears_approval_index() {
        let store = RuntimeStateStore::new(None, "test:".to_string());
        store.apply_event(&RuntimeEvent::ApprovalRequested {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            request_id: "approval-1".to_string(),
            permission: "runtime_echo".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "runtime_echo".to_string(),
            arguments: serde_json::Value::Null,
            message: "approval needed".to_string(),
        });

        let index = store
            .resolve_approval("approval-1")
            .expect("approval index");
        assert_eq!(index.conversation_id, "conv-1");
        assert_eq!(index.runtime_session_id, "session-1");
        assert_eq!(index.turn_id, "turn-1");

        store.apply_event(&RuntimeEvent::ApprovalResolved {
            conversation_id: "conv-1".to_string(),
            runtime_session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            request_id: "approval-1".to_string(),
            decision: "approve".to_string(),
        });

        assert!(store.resolve_approval("approval-1").is_none());
    }
}
