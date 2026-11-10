//! The id port. Generation (UUID v7) lives outside core.

use crate::plan::ItemId;

/// Mints ids for new local entities.
pub trait IdGen: Send + Sync + 'static {
    fn new_item_id(&self) -> ItemId;
}
