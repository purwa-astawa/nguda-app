//! The id port. Generation (UUID v7) lives outside core.

use std::fmt;

use uuid::Uuid;

use crate::plan::ItemId;

/// Identity of one prompt (a notification the engine decided to show). Renderers
/// post each prompt id at most once (AD-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PromptId(pub Uuid);

impl fmt::Display for PromptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Mints ids for new local entities.
pub trait IdGen: Send + Sync + 'static {
    fn new_item_id(&self) -> ItemId;
    fn new_prompt_id(&self) -> PromptId;
}
