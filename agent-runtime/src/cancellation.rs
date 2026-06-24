use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct CancellationRegistry {
    inner: Arc<Mutex<HashMap<String, CancelToken>>>,
}

#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    inner: Arc<Mutex<CancelState>>,
}

#[derive(Debug, Clone, Default)]
struct CancelState {
    cancelled: bool,
    reason: String,
    continue_after: bool,
}

impl CancellationRegistry {
    pub fn start(&self, conversation_id: &str) -> CancelToken {
        let token = CancelToken::default();
        self.inner
            .lock()
            .expect("cancellation registry poisoned")
            .insert(conversation_id.to_string(), token.clone());
        token
    }

    pub fn cancel(&self, conversation_id: &str, reason: String, continue_after: bool) -> bool {
        let Some(token) = self
            .inner
            .lock()
            .expect("cancellation registry poisoned")
            .get(conversation_id)
            .cloned()
        else {
            return false;
        };
        token.cancel(reason, continue_after);
        true
    }

    pub fn clear(&self, conversation_id: &str, token: &CancelToken) {
        let mut inner = self.inner.lock().expect("cancellation registry poisoned");
        if inner
            .get(conversation_id)
            .is_some_and(|current| current.same_token(token))
        {
            inner.remove(conversation_id);
        }
    }
}

impl CancelToken {
    pub fn cancel(&self, reason: String, continue_after: bool) {
        let mut state = self.inner.lock().expect("cancel token poisoned");
        state.cancelled = true;
        state.reason = reason;
        state.continue_after = continue_after;
    }

    pub fn abort_reason(&self) -> Option<String> {
        let state = self.inner.lock().expect("cancel token poisoned");
        if !state.cancelled {
            return None;
        }
        let mut reason = state.reason.trim().to_string();
        if reason.is_empty() {
            reason = "runtime turn interrupted".to_string();
        }
        if state.continue_after {
            reason = format!("interrupt_continue: {}", reason);
        }
        Some(reason)
    }

    fn same_token(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}
