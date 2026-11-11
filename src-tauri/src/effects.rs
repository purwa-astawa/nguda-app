//! The engine's effect consumer (AD-10): the engine decides, this task renders.
//!
//! For each `Notify` it looks up the task title in the latest snapshot and posts
//! once per `prompt_id`. Each `WithdrawNotification` is passed to the notifier;
//! the prompt id stays in `posted`, so a withdrawn prompt is never re-posted.
//! Failures are logged with ids and kinds only (AD-14), never titles. `Recovered`
//! needs no rendering yet and is ignored.

use std::collections::HashSet;
use std::sync::Arc;

use nguda_core::{ItemId, Notification, Notifier, PromptId, PromptKind};
use nguda_engine::{Effect, Snapshot};
use tokio::sync::{mpsc, watch};

/// Posts each prompt at most once.
struct Consumer {
    notifier: Arc<dyn Notifier>,
    posted: HashSet<PromptId>,
}

impl Consumer {
    fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            posted: HashSet::new(),
        }
    }

    fn handle(&mut self, effect: Effect, snapshot: &Snapshot) {
        match effect {
            Effect::Notify {
                prompt_id,
                item_id,
                kind,
            } => self.post(prompt_id, item_id, kind, snapshot),
            Effect::WithdrawNotification { prompt_id } => {
                if let Err(e) = self.notifier.withdraw(prompt_id) {
                    // NotifierError messages carry ids and OS errors only.
                    eprintln!("nguda: failed to withdraw prompt {prompt_id}: {e}");
                }
            }
            Effect::Recovered { .. } => {}
        }
    }

    fn post(
        &mut self,
        prompt_id: PromptId,
        item_id: ItemId,
        kind: PromptKind,
        snapshot: &Snapshot,
    ) {
        if !self.posted.insert(prompt_id) {
            return;
        }
        let Some(item) = snapshot.day.as_ref().and_then(|d| d.item(item_id)) else {
            eprintln!(
                "nguda: prompt {prompt_id} ({}) for unknown item {item_id}; not posted",
                kind_name(kind)
            );
            return;
        };
        let note = Notification {
            prompt_id,
            kind,
            task_title: item.title().to_owned(),
        };
        if let Err(e) = self.notifier.post(&note) {
            // NotifierError messages carry ids and OS errors only.
            eprintln!(
                "nguda: failed to post prompt {prompt_id} ({}) for item {item_id}: {e}",
                kind_name(kind)
            );
        }
    }
}

fn kind_name(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::PlannedEnd => "planned_end",
    }
}

/// Renders effects until the engine stops.
pub async fn consume(
    mut effects: mpsc::UnboundedReceiver<Effect>,
    snapshots: watch::Receiver<Snapshot>,
    notifier: Arc<dyn Notifier>,
) {
    let mut consumer = Consumer::new(notifier);
    while let Some(effect) = effects.recv().await {
        // Copy out, so the lock is not held while posting.
        let snapshot = snapshots.borrow().clone();
        consumer.handle(effect, &snapshot);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use nguda_core::{Date, Day, NotifierError, OrderKey, PlanItem, Uuid};

    use super::*;

    #[derive(Default)]
    struct FakeNotifier {
        fail: bool,
        posted: Mutex<Vec<Notification>>,
        withdrawn: Mutex<Vec<PromptId>>,
    }

    impl Notifier for FakeNotifier {
        fn post(&self, n: &Notification) -> Result<(), NotifierError> {
            self.posted.lock().unwrap().push(n.clone());
            if self.fail {
                Err(NotifierError::new("boom"))
            } else {
                Ok(())
            }
        }

        fn withdraw(&self, prompt_id: PromptId) -> Result<(), NotifierError> {
            self.withdrawn.lock().unwrap().push(prompt_id);
            if self.fail {
                Err(NotifierError::new("boom"))
            } else {
                Ok(())
            }
        }
    }

    fn item_id() -> ItemId {
        ItemId(Uuid::from_u128(1))
    }

    fn prompt(n: u128) -> PromptId {
        PromptId(Uuid::from_u128(100 + n))
    }

    fn snapshot() -> Snapshot {
        let mut day = Day::new(Date::constant(2026, 11, 11), "UTC");
        day.add(PlanItem::new(
            item_id(),
            "Write",
            60,
            OrderKey::new("V").unwrap(),
        ))
        .unwrap();
        Snapshot {
            rev: 2,
            day: Some(day),
            ..Default::default()
        }
    }

    fn notify(n: u128) -> Effect {
        Effect::Notify {
            prompt_id: prompt(n),
            item_id: item_id(),
            kind: PromptKind::PlannedEnd,
        }
    }

    #[test]
    fn a_prompt_is_posted_once_with_the_task_title() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone());
        c.handle(notify(1), &snapshot());
        c.handle(notify(1), &snapshot());
        c.handle(notify(2), &snapshot());
        let posted = fake.posted.lock().unwrap().clone();
        assert_eq!(
            posted,
            vec![
                Notification {
                    prompt_id: prompt(1),
                    kind: PromptKind::PlannedEnd,
                    task_title: "Write".into(),
                },
                Notification {
                    prompt_id: prompt(2),
                    kind: PromptKind::PlannedEnd,
                    task_title: "Write".into(),
                },
            ]
        );
    }

    #[test]
    fn a_failed_post_is_not_retried() {
        let fake = Arc::new(FakeNotifier {
            fail: true,
            ..Default::default()
        });
        let mut c = Consumer::new(fake.clone());
        c.handle(notify(1), &snapshot());
        c.handle(notify(1), &snapshot());
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }

    #[test]
    fn recovered_and_unknown_items_post_nothing() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone());
        c.handle(Effect::Recovered { item_id: item_id() }, &snapshot());
        c.handle(notify(1), &Snapshot::default());
        assert!(fake.posted.lock().unwrap().is_empty());
    }

    #[test]
    fn withdraw_reaches_the_notifier_once_and_never_re_posts() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone());
        c.handle(notify(1), &snapshot());
        c.handle(
            Effect::WithdrawNotification {
                prompt_id: prompt(1),
            },
            &snapshot(),
        );
        c.handle(notify(1), &snapshot());
        assert_eq!(*fake.withdrawn.lock().unwrap(), vec![prompt(1)]);
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_failed_withdraw_is_logged_and_keeps_the_prompt_posted() {
        let fake = Arc::new(FakeNotifier {
            fail: true,
            ..Default::default()
        });
        let mut c = Consumer::new(fake.clone());
        c.handle(notify(1), &snapshot());
        c.handle(
            Effect::WithdrawNotification {
                prompt_id: prompt(1),
            },
            &snapshot(),
        );
        c.handle(notify(1), &snapshot());
        assert_eq!(*fake.withdrawn.lock().unwrap(), vec![prompt(1)]);
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn consume_posts_until_the_engine_stops() {
        let fake = Arc::new(FakeNotifier::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let (_snap_tx, snaps) = watch::channel(snapshot());
        tx.send(notify(1)).unwrap();
        tx.send(notify(1)).unwrap();
        drop(tx);
        consume(rx, snaps, fake.clone()).await;
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }
}
