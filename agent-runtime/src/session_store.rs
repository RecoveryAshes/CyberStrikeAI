use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::compaction::CompactionArtifact;
use crate::mcp_registry::LoadedToolRecord;
use crate::model_stream::{ChatMessage, ModelToolCall};
use crate::plan_store::PlanItem;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StoredSession {
    pub runtime_session_id: String,
    pub conversation_id: String,
    pub last_turn_id: Option<String>,
    pub active_turn_id: Option<String>,
    pub turn_count: u64,
    pub state_summary: String,
    pub updated_at_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_approval: Option<StoredPendingApproval>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_artifacts: Vec<StoredCompactionArtifactRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub compaction_tasks: Vec<StoredCompactionTask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_loaded_tools: Vec<LoadedToolRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredPendingApproval {
    pub request_id: String,
    pub turn_id: String,
    pub tool_call: ModelToolCall,
    pub messages: Vec<ChatMessage>,
    pub plan_items: Vec<PlanItem>,
    pub context: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCompactionArtifactRef {
    pub task_id: String,
    pub strategy: String,
    pub path: String,
    pub input_message_count: usize,
    pub input_chars: usize,
    #[serde(default)]
    pub replacement_message_count: usize,
    #[serde(default)]
    pub summary_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredCompactionTask {
    pub task_id: String,
    pub strategy: String,
    pub status: String,
    pub artifact_path: String,
    pub input_message_count: usize,
    pub input_chars: usize,
    #[serde(default)]
    pub replacement_message_count: usize,
    #[serde(default)]
    pub summary_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    root: Option<PathBuf>,
    active_lock_root: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ActiveRunGuard {
    #[cfg(unix)]
    file: Option<File>,
    #[cfg(not(unix))]
    path: Option<PathBuf>,
}

impl SessionStore {
    pub fn from_context(context: &Map<String, Value>) -> Self {
        let root = context
            .get("session_store_dir")
            .or_else(|| context.get("runtime_session_store_dir"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let active_lock_root = root.clone().or_else(default_active_lock_root);
        Self {
            root,
            active_lock_root,
        }
    }

    pub fn load(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
    ) -> io::Result<Option<StoredSession>> {
        let Some(path) = self.session_path(conversation_id, runtime_session_id) else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let session = serde_json::from_str(&raw)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(Some(session))
    }

    pub fn save(&self, session: &StoredSession) -> io::Result<()> {
        let Some(path) = self.session_path(&session.conversation_id, &session.runtime_session_id)
        else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(session)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        fs::write(path, raw)?;
        Ok(())
    }

    pub fn save_compaction_artifact(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
        artifact: &CompactionArtifact,
    ) -> io::Result<Option<StoredCompactionArtifactRef>> {
        let Some(path) =
            self.compaction_artifact_path(conversation_id, runtime_session_id, &artifact.task_id)
        else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(artifact)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        fs::write(&path, raw)?;
        Ok(Some(StoredCompactionArtifactRef {
            task_id: artifact.task_id.clone(),
            strategy: artifact.strategy.clone(),
            path: path.to_string_lossy().to_string(),
            input_message_count: artifact.input_message_count,
            input_chars: artifact.input_chars,
            replacement_message_count: artifact.replacement_messages.len(),
            summary_source: artifact.summary_source.clone(),
            summary_error: artifact.summary_error.clone(),
        }))
    }

    pub fn claim_active_run(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
        turn_id: &str,
    ) -> io::Result<ActiveRunGuard> {
        let Some(path) = self.active_run_lock_path(conversation_id) else {
            return Ok(ActiveRunGuard::noop());
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(&path)?;
            try_lock_exclusive(&file).map_err(|err| {
                if err.kind() == io::ErrorKind::WouldBlock {
                    io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "conversation already has an active runtime submission",
                    )
                } else {
                    err
                }
            })?;
            file.set_len(0)?;
            writeln!(
                file,
                "conversation_id={}\nruntime_session_id={}\nturn_id={}\nupdated_at_unix={}",
                conversation_id,
                runtime_session_id,
                turn_id,
                now_unix()
            )?;
            file.sync_data()?;
            Ok(ActiveRunGuard { file: Some(file) })
        }
        #[cfg(not(unix))]
        {
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|err| {
                    if err.kind() == io::ErrorKind::AlreadyExists {
                        io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "conversation already has an active runtime submission",
                        )
                    } else {
                        err
                    }
                })?;
            writeln!(
                file,
                "conversation_id={}\nruntime_session_id={}\nturn_id={}\nupdated_at_unix={}",
                conversation_id,
                runtime_session_id,
                turn_id,
                now_unix()
            )?;
            Ok(ActiveRunGuard { path: Some(path) })
        }
    }

    fn session_path(&self, conversation_id: &str, runtime_session_id: &str) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        Some(
            root.join(sanitize_segment(conversation_id))
                .join(format!("{}.json", sanitize_segment(runtime_session_id))),
        )
    }

    fn compaction_artifact_path(
        &self,
        conversation_id: &str,
        runtime_session_id: &str,
        task_id: &str,
    ) -> Option<PathBuf> {
        let root = self.root.as_ref()?;
        Some(
            root.join(sanitize_segment(conversation_id))
                .join("compactions")
                .join(sanitize_segment(runtime_session_id))
                .join(format!("{}.json", sanitize_segment(task_id))),
        )
    }

    fn active_run_lock_path(&self, conversation_id: &str) -> Option<PathBuf> {
        let root = self.active_lock_root.as_ref()?;
        Some(
            root.join(sanitize_segment(conversation_id))
                .join(".active-run.lock"),
        )
    }
}

impl ActiveRunGuard {
    fn noop() -> Self {
        Self {
            #[cfg(unix)]
            file: None,
            #[cfg(not(unix))]
            path: None,
        }
    }
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(file) = self.file.take() {
            let _ = unlock_exclusive(&file);
        }
        #[cfg(not(unix))]
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

impl StoredSession {
    pub fn new(conversation_id: String, runtime_session_id: String) -> Self {
        Self {
            conversation_id,
            runtime_session_id,
            last_turn_id: None,
            active_turn_id: None,
            turn_count: 0,
            state_summary: "created".to_string(),
            updated_at_unix: now_unix(),
            pending_approval: None,
            compaction_artifacts: Vec::new(),
            compaction_tasks: Vec::new(),
            mcp_loaded_tools: Vec::new(),
        }
    }

    pub fn mark_active(&mut self, turn_id: String) {
        self.active_turn_id = Some(turn_id);
        self.state_summary = "active".to_string();
        self.updated_at_unix = now_unix();
        self.pending_approval = None;
    }

    pub fn mark_finished(&mut self, turn_id: String, state_summary: impl Into<String>) {
        self.active_turn_id = None;
        self.last_turn_id = Some(turn_id);
        self.turn_count = self.turn_count.saturating_add(1);
        self.state_summary = state_summary.into();
        self.updated_at_unix = now_unix();
        self.pending_approval = None;
    }

    pub fn mark_pending_approval(&mut self, pending: StoredPendingApproval) {
        self.active_turn_id = Some(pending.turn_id.clone());
        self.state_summary = "pending_approval".to_string();
        self.updated_at_unix = now_unix();
        self.pending_approval = Some(pending);
    }

    pub fn append_compaction_artifacts(&mut self, mut artifacts: Vec<StoredCompactionArtifactRef>) {
        if artifacts.is_empty() {
            return;
        }
        for artifact in &artifacts {
            self.compaction_tasks.push(StoredCompactionTask {
                task_id: artifact.task_id.clone(),
                strategy: artifact.strategy.clone(),
                status: "completed".to_string(),
                artifact_path: artifact.path.clone(),
                input_message_count: artifact.input_message_count,
                input_chars: artifact.input_chars,
                replacement_message_count: artifact.replacement_message_count,
                summary_source: artifact.summary_source.clone(),
                summary_error: artifact.summary_error.clone(),
            });
        }
        self.compaction_artifacts.append(&mut artifacts);
        self.updated_at_unix = now_unix();
    }

    pub fn set_mcp_loaded_tools(&mut self, records: Vec<LoadedToolRecord>) {
        self.mcp_loaded_tools = records;
        self.updated_at_unix = now_unix();
    }
}

#[cfg(unix)]
fn try_lock_exclusive(file: &File) -> io::Result<()> {
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlock_exclusive(file: &File) -> io::Result<()> {
    const LOCK_UN: i32 = 8;
    let result = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn sanitize_segment(segment: &str) -> String {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return "unknown".to_string();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn default_active_lock_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    Some(
        std::env::temp_dir()
            .join("cyberstrike-agent-runtime")
            .join("active-runs")
            .join(stable_path_hash(&cwd.to_string_lossy())),
    )
}

fn stable_path_hash(value: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn saves_and_loads_session() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-store-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let store = SessionStore::from_context(&context);
        let mut session = StoredSession::new("conv/1".to_string(), "session:1".to_string());
        session.mark_active("turn-1".to_string());
        session.mark_finished("turn-1".to_string(), "completed");
        store.save(&session).unwrap();

        let loaded = store.load("conv/1", "session:1").unwrap().unwrap();
        assert_eq!(loaded.runtime_session_id, "session:1");
        assert_eq!(loaded.turn_count, 1);
        assert_eq!(loaded.state_summary, "completed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saves_and_loads_mcp_loaded_state() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-mcp-loaded-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let store = SessionStore::from_context(&context);
        let mut session = StoredSession::new("conv/1".to_string(), "session:1".to_string());
        session.set_mcp_loaded_tools(vec![LoadedToolRecord {
            identity: "builtin::nmap".to_string(),
            state: crate::mcp_registry::LoadedToolStatus::BudgetBlocked,
            selected_at: 10,
            last_used_at: 11,
            used_count: 2,
            schema_hash: "hash".to_string(),
        }]);
        store.save(&session).unwrap();

        let loaded = store.load("conv/1", "session:1").unwrap().unwrap();

        assert_eq!(loaded.mcp_loaded_tools.len(), 1);
        assert_eq!(loaded.mcp_loaded_tools[0].identity, "builtin::nmap");
        assert_eq!(loaded.mcp_loaded_tools[0].used_count, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn saves_compaction_artifact_and_session_reference() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-compaction-store-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let store = SessionStore::from_context(&context);
        let artifact = CompactionArtifact {
            task_id: "compaction_turn-1".to_string(),
            strategy: "rollout_summary_with_recent_tail".to_string(),
            input_message_count: 1,
            input_chars: 7,
            input_messages: vec![ChatMessage::user("before")],
            summary_attempts: Vec::new(),
            summary_source: "local_heuristic".to_string(),
            summary_error: None,
            summary: "summary".to_string(),
            replacement_metadata: crate::compaction::CompactionReplacementMetadata::default(),
            replacement_messages: vec![ChatMessage::system("after")],
        };
        let artifact_ref = store
            .save_compaction_artifact("conv/1", "session:1", &artifact)
            .unwrap()
            .unwrap();
        assert!(artifact_ref.path.ends_with("compaction_turn-1.json"));
        let raw = fs::read_to_string(&artifact_ref.path).unwrap();
        assert!(raw.contains("\"input_messages\""));
        assert!(raw.contains("\"replacement_messages\""));

        let mut session = StoredSession::new("conv/1".to_string(), "session:1".to_string());
        session.append_compaction_artifacts(vec![artifact_ref]);
        store.save(&session).unwrap();
        let loaded = store.load("conv/1", "session:1").unwrap().unwrap();
        assert_eq!(loaded.compaction_artifacts.len(), 1);
        assert_eq!(loaded.compaction_artifacts[0].task_id, "compaction_turn-1");
        assert_eq!(loaded.compaction_tasks.len(), 1);
        assert_eq!(loaded.compaction_tasks[0].status, "completed");
        assert_eq!(
            loaded.compaction_tasks[0].artifact_path,
            loaded.compaction_artifacts[0].path
        );
        assert_eq!(loaded.compaction_artifacts[0].replacement_message_count, 1);
        assert_eq!(
            loaded.compaction_artifacts[0].summary_source,
            "local_heuristic"
        );
        assert_eq!(loaded.compaction_tasks[0].replacement_message_count, 1);
        assert_eq!(loaded.compaction_tasks[0].summary_source, "local_heuristic");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn active_run_guard_rejects_same_conversation_until_released() {
        let root = std::env::temp_dir().join(format!(
            "cyberstrike-session-active-run-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let mut context = Map::new();
        context.insert(
            "session_store_dir".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let store = SessionStore::from_context(&context);

        let first = store
            .claim_active_run("conv/1", "session:1", "turn-1")
            .unwrap();
        let duplicate = store.claim_active_run("conv/1", "session:2", "turn-2");
        assert_eq!(duplicate.unwrap_err().kind(), io::ErrorKind::WouldBlock);

        let other = store
            .claim_active_run("conv/2", "session:2", "turn-3")
            .unwrap();
        drop(other);
        drop(first);

        store
            .claim_active_run("conv/1", "session:1", "turn-4")
            .unwrap();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn active_run_guard_uses_default_lock_root_without_session_store() {
        let store = SessionStore::from_context(&Map::new());

        let first = store
            .claim_active_run("direct-conv", "session-1", "turn-1")
            .unwrap();
        let duplicate = store.claim_active_run("direct-conv", "session-2", "turn-2");

        assert_eq!(duplicate.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(first);
    }
}
