//! The notifier port (AD-10). The engine decides when to prompt; a notifier only
//! renders, posting each prompt once.

use std::fmt;

use crate::ids::PromptId;

/// Why the engine prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptKind {
    /// The running item reached its planned length.
    PlannedEnd,
}

/// One notification to show. Its text may name the task (AD-14 restricts logs, not
/// the notification itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub prompt_id: PromptId,
    pub kind: PromptKind,
    pub task_title: String,
}

/// A notifier failure. The message carries ids and OS errors only, never titles.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifierError {
    pub message: String,
}

impl NotifierError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for NotifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for NotifierError {}

/// Renders notifications. `post` must not block on the user.
pub trait Notifier: Send + Sync + 'static {
    fn post(&self, n: &Notification) -> Result<(), NotifierError>;
}
