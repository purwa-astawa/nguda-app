//! The notifier port (AD-10). The engine decides when to prompt; a notifier only
//! renders, posting each prompt once.

use std::fmt;

use crate::ids::PromptId;

/// Why the engine prompts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptKind {
    /// The running item reached its planned length.
    PlannedEnd,
    /// Re-flow pushed flexible work past the work-end time.
    Overflow,
}

/// A button on a notification. Each one acts like the tray item of the same name
/// (AD-10): the shell maps it to a targeted command for the prompt's item (AD-15).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PromptAction {
    /// Extend the item by 5 minutes.
    Extend5,
    /// Extend the item by 15 minutes.
    Extend15,
    /// Open the window's extend field for a custom length.
    ExtendCustom,
    /// Finish the item and move on.
    Switch,
}

impl PromptAction {
    /// Every action, in the order a notification shows them.
    pub const ALL: [PromptAction; 4] = [
        PromptAction::Extend5,
        PromptAction::Extend15,
        PromptAction::ExtendCustom,
        PromptAction::Switch,
    ];
}

/// One notification to show. Its text may name the task (AD-14 restricts logs, not
/// the notification itself).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub prompt_id: PromptId,
    pub kind: PromptKind,
    pub task_title: String,
    /// The buttons to offer, in order. Empty: a plain banner.
    pub actions: Vec<PromptAction>,
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

/// Renders notifications. Neither method may block on the user.
pub trait Notifier: Send + Sync + 'static {
    fn post(&self, n: &Notification) -> Result<(), NotifierError>;
    /// Removes the prompt `prompt_id` posted earlier, if the platform can. Required,
    /// so each adapter decides what withdrawing means for it.
    fn withdraw(&self, prompt_id: PromptId) -> Result<(), NotifierError>;
}
