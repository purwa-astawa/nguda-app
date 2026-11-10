//! Domain rule violations.

use std::fmt;

use crate::plan::{Action, ItemId, ItemStatus};

/// Why a domain operation was rejected. A rejected operation leaves its input unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DomainError {
    /// The day already has an open segment, so nothing else can start or resume (AD-6).
    AnotherItemRunning,
    /// `action` is not allowed from the item's current state (AD-15).
    InvalidTransition { from: ItemStatus, action: Action },
    /// Order keys are malformed or not strictly increasing.
    InvalidOrder,
    /// No item with this id exists in the day.
    UnknownItem(ItemId),
    /// The day already has an item with this id.
    DuplicateItem(ItemId),
    /// A stored record is internally inconsistent and cannot be rehydrated.
    InvalidRecord { id: ItemId, reason: &'static str },
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AnotherItemRunning => f.write_str("another item is already running"),
            Self::InvalidTransition { from, action } => {
                write!(f, "cannot {action} an item that is {from}")
            }
            Self::InvalidOrder => f.write_str("invalid order keys"),
            Self::UnknownItem(id) => write!(f, "unknown item {id}"),
            Self::DuplicateItem(id) => write!(f, "duplicate item {id}"),
            Self::InvalidRecord { id, reason } => {
                write!(f, "invalid record for item {id}: {reason}")
            }
        }
    }
}

impl std::error::Error for DomainError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_the_state_and_action() {
        let err = DomainError::InvalidTransition {
            from: ItemStatus::Pending,
            action: Action::Pause,
        };
        assert_eq!(err.to_string(), "cannot pause an item that is pending");
        assert_eq!(
            DomainError::AnotherItemRunning.to_string(),
            "another item is already running"
        );
    }
}
