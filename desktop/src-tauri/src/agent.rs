//! The state every tool shares, and the shape every tool returns.

use crate::audit::Audit;
use crate::guard::Guard;
use crate::policy::Policy;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::RwLock;

/// What a tool hands back to the frontend.
///
/// One type for reads and writes, because the frontend already has one code path
/// for both: a read uses `text`, a write checks `ok`. Errors do not travel in
/// here at all — they come back as a rejected promise carrying a sentence, which
/// is what the chat layer already knows how to show and what the model needs in
/// order to explain the refusal rather than retry it.
#[derive(Debug, Serialize)]
pub struct ToolOut {
    pub ok: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
}

impl ToolOut {
    /// A read: text for the model.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            ok: true,
            text: text.into(),
        }
    }
    /// A write that reports what it did — the folder it created, the name it had
    /// to pick because the obvious one was taken. Writes always say something,
    /// because "done" with no detail is how a silent wrong action hides.
    pub fn done_with(text: impl Into<String>) -> Self {
        Self {
            ok: true,
            text: text.into(),
        }
    }
}

pub struct Agent {
    policy: RwLock<Policy>,
    /// Resolved once at startup: the app's own directories, which no tool may
    /// touch. Computed here rather than per call so a tool cannot be handed a
    /// different idea of what is denied than the one the app started with.
    denied: Vec<PathBuf>,
    home: Option<PathBuf>,
    pub audit: Audit,
}

impl Agent {
    pub fn new(policy: Policy, denied: Vec<PathBuf>, home: Option<PathBuf>, audit: Audit) -> Self {
        Self {
            policy: RwLock::new(policy),
            denied,
            home,
            audit,
        }
    }

    /// A snapshot of the policy. Tools take a copy rather than holding the lock,
    /// so a long file walk cannot block the settings being read.
    pub fn policy(&self) -> Policy {
        self.policy
            .read()
            .map(|p| p.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Re-read the policy file from disk. Called when the user has edited it, so
    /// widening or narrowing the allowed folders does not need a restart.
    pub fn reload(&self, app: &tauri::AppHandle) {
        let fresh = Policy::load(app);
        if let Ok(mut w) = self.policy.write() {
            *w = fresh;
        }
    }

    /// A guard built from the current policy. Cheap, and deliberately per call:
    /// it means an edit to the allowed folders takes effect on the very next
    /// tool, and that no tool can cache a wider sandbox than it should have.
    pub fn guard(&self) -> Guard {
        Guard::new(&self.policy(), self.denied.clone(), self.home.clone())
    }
}
