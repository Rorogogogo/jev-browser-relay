//! Multiple concurrent sessions, addressed by id.
//!
//! Each session owns a browser backend, so they are independent. The registry holds each behind
//! its own mutex: two sessions never block each other, and two calls against the *same* session
//! serialize, which is what stops a `run` and a `provide_input` racing on one browser.

use crate::error::{RelayError, Result};
use crate::session::Session;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Arc<Mutex<Session>>>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, session: Session) -> Arc<Mutex<Session>> {
        let id = session.id.clone();
        let handle = Arc::new(Mutex::new(session));
        self.sessions.lock().await.insert(id, handle.clone());
        handle
    }

    pub async fn get(&self, id: &str) -> Result<Arc<Mutex<Session>>> {
        self.sessions.lock().await.get(id).cloned().ok_or_else(|| RelayError::UnknownSession(id.to_string()))
    }

    pub async fn remove(&self, id: &str) -> Result<Arc<Mutex<Session>>> {
        self.sessions.lock().await.remove(id).ok_or_else(|| RelayError::UnknownSession(id.to_string()))
    }

    pub async fn ids(&self) -> Vec<String> {
        self.sessions.lock().await.keys().cloned().collect()
    }

    pub async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Drop sessions idle beyond the timeout, closing their browsers.
    pub async fn reap_idle(&self, idle_timeout_ms: u64) -> Vec<String> {
        let candidates: Vec<(String, Arc<Mutex<Session>>)> =
            self.sessions.lock().await.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut reaped = Vec::new();
        for (id, handle) in candidates {
            // Skip anything mid-call rather than waiting on it.
            let Ok(mut session) = handle.try_lock() else { continue };
            if session.idle_ms() >= idle_timeout_ms {
                let _ = session.stop().await;
                reaped.push(id);
            }
        }
        if !reaped.is_empty() {
            let mut sessions = self.sessions.lock().await;
            for id in &reaped {
                sessions.remove(id);
            }
        }
        reaped
    }
}

pub fn new_session_id() -> String {
    format!("ses_{}", uuid::Uuid::new_v4().simple())
}
