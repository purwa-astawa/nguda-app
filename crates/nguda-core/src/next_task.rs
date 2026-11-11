//! What runs next (AD-15): the interrupt stack first, then the schedule.

use jiff::Timestamp;

use crate::plan::{Day, ItemId};
use crate::reflow::reflow;

/// The item to run next in `day` as of `now`: the top of the interrupt stack if
/// there is one, else the first pending or paused item of [`reflow`]'s schedule,
/// or `None` when nothing is left. Pure.
pub fn next_task(day: &Day, now: Timestamp) -> Option<ItemId> {
    day.interrupt_stack()
        .last()
        .copied()
        .or_else(|| reflow(day, now).next())
}
