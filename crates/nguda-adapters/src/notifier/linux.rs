//! Linux: `notify-rust` over D-Bus (its zbus backend).
//!
//! A notification with actions gets its buttons only when the server's
//! capabilities include `actions`; otherwise it posts without them. Each prompt
//! is shown from its own thread, so `post` never blocks on D-Bus. A notification
//! with buttons is kept by prompt id and that thread waits until the user picks
//! an action or it closes; a picked action goes to the callback. Withdraw closes
//! it by its server id (`CloseNotification`), which also ends the wait. One
//! without buttons is dropped once shown and cannot be withdrawn.

use std::collections::HashMap;
use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use nguda_core::{Notification, NotifierError, PromptId};
use notify_rust::{NotificationHandle, NotificationResponse};

use super::{APP_NAME, ActionCallback, action_from_id, action_id, action_label, headline};

/// A prompt between `post` and its close: `Posting` until its thread has shown
/// it, then `Shown` while it waits for an action.
enum Entry {
    Posting,
    Shown(Arc<NotificationHandle>),
}

/// Prompts by id, so withdraw can close them. A prompt withdrawn while still
/// `Posting` is simply removed; its thread then closes it right after showing.
type Open = Arc<Mutex<HashMap<PromptId, Entry>>>;

pub(super) struct Backend {
    on_action: ActionCallback,
    open: Open,
}

impl Backend {
    pub(super) fn new(on_action: ActionCallback) -> Self {
        Self {
            on_action,
            open: Arc::default(),
        }
    }

    /// Only spawns: the capability query, the show and the wait are blocking D-Bus
    /// calls, so they run on the prompt's own thread, never the caller's. A show
    /// failure there is logged with the prompt id only (AD-14).
    pub(super) fn post(&self, n: &Notification) -> Result<(), NotifierError> {
        let prompt_id = n.prompt_id;
        lock(&self.open).insert(prompt_id, Entry::Posting);
        let (n, open, on_action) = (n.clone(), self.open.clone(), self.on_action.clone());
        let thread_open = open.clone();
        let spawned = thread::Builder::new()
            .name("nguda-notifier-wait".into())
            .spawn(move || run(n, thread_open, on_action));
        if let Err(e) = spawned {
            lock(&open).remove(&prompt_id);
            return Err(NotifierError::new(format!(
                "prompt {prompt_id}: cannot post: {e}"
            )));
        }
        Ok(())
    }

    /// Closes the notification by its server id, off the caller's thread. A prompt
    /// still being posted is closed by its own thread once shown; one already
    /// closed, without buttons, or never posted is a no-op.
    pub(super) fn withdraw(&self, prompt_id: PromptId) -> Result<(), NotifierError> {
        let Some(Entry::Shown(handle)) = lock(&self.open).remove(&prompt_id) else {
            return Ok(());
        };
        thread::Builder::new()
            .name("nguda-notifier-close".into())
            .spawn(move || block_on(handle.close_async()))
            .map(drop)
            .map_err(|e| NotifierError::new(format!("prompt {prompt_id}: cannot close: {e}")))
    }
}

/// The prompt's thread: shows `n`, then, only if buttons were added, waits for
/// one action or a close and reports the action. A notification without buttons
/// is dropped right after showing, so it keeps no connection or thread.
fn run(n: Notification, open: Open, on_action: ActionCallback) {
    let prompt_id = n.prompt_id;
    let with_actions = !n.actions.is_empty() && supports_actions(prompt_id);
    let handle = match build(&n, with_actions).show() {
        Ok(h) => Arc::new(h),
        Err(e) => {
            lock(&open).remove(&prompt_id);
            eprintln!("nguda: prompt {prompt_id}: cannot post: {e}");
            return;
        }
    };
    {
        let mut open = lock(&open);
        if open.remove(&prompt_id).is_none() {
            // Withdrawn while posting.
            drop(open);
            block_on(handle.close_async());
            return;
        }
        if !with_actions {
            return;
        }
        open.insert(prompt_id, Entry::Shown(handle.clone()));
    }
    let mut picked = None;
    block_on(handle.wait_for_action_async(|response| {
        if let NotificationResponse::Action(id) = response {
            picked = action_from_id(id);
        }
    }));
    lock(&open).remove(&prompt_id);
    drop(handle);
    if let Some(action) = picked {
        on_action(prompt_id, action);
    }
}

/// Whether the notification server can show action buttons. A failed query
/// counts as no, logged with the prompt id only.
fn supports_actions(prompt_id: PromptId) -> bool {
    match notify_rust::get_capabilities() {
        Ok(caps) => caps.iter().any(|c| c == "actions"),
        Err(e) => {
            eprintln!("nguda: prompt {prompt_id}: cannot read notification capabilities: {e}");
            false
        }
    }
}

/// The notification for `n`, with its actions only when `with_actions`.
fn build(n: &Notification, with_actions: bool) -> notify_rust::Notification {
    let mut note = notify_rust::Notification::new();
    note.appname(APP_NAME)
        .summary(headline(n.kind))
        .body(&n.task_title);
    if with_actions {
        for action in &n.actions {
            note.action(action_id(*action), action_label(*action));
        }
    }
    note
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `fut` to completion on this thread. zbus drives its connection on its own
/// executor thread, so parking here until woken is enough.
fn block_on<F: Future>(fut: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return out;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use nguda_core::{PromptAction, PromptKind, Uuid};

    use super::*;

    fn note(kind: PromptKind, actions: Vec<PromptAction>) -> Notification {
        Notification {
            prompt_id: PromptId(Uuid::from_u128(7)),
            kind,
            task_title: "Write report".into(),
            actions,
        }
    }

    #[test]
    fn planned_end_offers_the_four_actions_when_supported() {
        let built = build(
            &note(PromptKind::PlannedEnd, PromptAction::ALL.to_vec()),
            true,
        );
        assert_eq!(built.summary, "Planned time is up");
        assert_eq!(built.body, "Write report");
        assert_eq!(
            built.actions,
            [
                "extend5",
                "Extend +5 min",
                "extend15",
                "Extend +15 min",
                "extend-custom",
                "Extend…",
                "switch",
                "Switch"
            ]
        );
    }

    #[test]
    fn without_server_support_the_actions_are_left_out() {
        let built = build(
            &note(PromptKind::PlannedEnd, PromptAction::ALL.to_vec()),
            false,
        );
        assert!(built.actions.is_empty());
    }

    #[test]
    fn a_notification_without_actions_has_no_buttons() {
        let built = build(&note(PromptKind::Overflow, vec![]), true);
        assert_eq!(built.summary, "Plan runs past work-end");
        assert!(built.actions.is_empty());
    }

    #[test]
    fn block_on_runs_a_ready_future() {
        assert_eq!(block_on(async { 7 }), 7);
    }
}
