//! The engine's effect consumer (AD-10): the engine decides, this task renders.
//!
//! For each `Notify` it looks up the task title in the latest snapshot and posts
//! once per `prompt_id`; an overflow prompt names the first leftover, plus how many
//! more the snapshot's overflow list holds. A planned-end prompt offers the four
//! actions (Extend +5, Extend +15, Extend…, Switch); an overflow prompt none. Each
//! `WithdrawNotification` is passed to the notifier; the prompt id stays in
//! `posted`, so a withdrawn prompt is never re-posted. Failures are logged with
//! ids and kinds only (AD-14), never titles. `Recovered` needs no rendering yet
//! and is ignored.
//!
//! Actions (AD-15): when it posts, the consumer records the prompt's target, the
//! item and the rev of the snapshot it rendered from, in [`PromptTargets`]. The
//! notifier reports a click as `(prompt_id, action)`, and [`resolve`] turns it into
//! the same targeted `Switch`/`ExtendTask` the tray sends, built from that record
//! and never from the current snapshot, so a click after a later commit is
//! rejected by the engine's rev check. Extend… sends nothing: like the tray's, it
//! opens the window's extend field. An unrecorded prompt is logged and dropped.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use nguda_core::{ItemId, Notification, Notifier, PromptAction, PromptId, PromptKind};
use nguda_engine::{Effect, EngineMsg, Snapshot};
use tokio::sync::{mpsc, watch};

use crate::tray::{EXTEND_5_SECS, EXTEND_15_SECS, extend_target, switch_target};

/// The item a prompt is about and the rev of the snapshot it was rendered from.
pub type Target = (ItemId, u64);

/// Targets by prompt, shared between the consumer (which records them when it
/// posts) and the notifier's action callback (which reads them). The lock is held
/// only to copy in or out. Entries are kept for the session, withdrawn prompts
/// included: a late click on a banner the platform could not remove still reaches
/// the engine and is rejected there as stale, rather than vanishing silently.
#[derive(Clone, Default)]
pub struct PromptTargets(Arc<Mutex<HashMap<PromptId, Target>>>);

impl PromptTargets {
    fn lock(&self) -> MutexGuard<'_, HashMap<PromptId, Target>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn record(&self, prompt_id: PromptId, target: Target) {
        self.lock().insert(prompt_id, target);
    }

    fn get(&self, prompt_id: PromptId) -> Option<Target> {
        self.lock().get(&prompt_id).copied()
    }
}

/// What a notification action does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActionOutcome {
    /// Send this message to the engine (fire-and-forget, AD-4).
    Send(EngineMsg),
    /// Bring the window forward, and focus the running row's extend field only
    /// while `item_id` (the prompt's item) is still the running item
    /// ([`focuses_extend`]); otherwise just show the window.
    FocusExtend { item_id: ItemId },
}

/// The outcome of `action` on a prompt with `target`: the tray's message shapes,
/// at the recorded item and rev.
pub fn action_msg(target: Target, action: PromptAction) -> ActionOutcome {
    let (item_id, rev) = target;
    match action {
        PromptAction::Extend5 => ActionOutcome::Send(extend_target(item_id, EXTEND_5_SECS, rev)),
        PromptAction::Extend15 => ActionOutcome::Send(extend_target(item_id, EXTEND_15_SECS, rev)),
        PromptAction::ExtendCustom => ActionOutcome::FocusExtend { item_id },
        PromptAction::Switch => ActionOutcome::Send(switch_target(item_id, rev)),
    }
}

/// Whether Extend… for a prompt about `item_id` may focus the extend field: only
/// while that item is the running one in `current`. A stale banner (its item no
/// longer running) only shows the window, so it never focuses another item's field.
pub fn focuses_extend(item_id: ItemId, current: &Snapshot) -> bool {
    current
        .running
        .as_ref()
        .is_some_and(|r| r.item_id == item_id)
}

/// The outcome of a click on `prompt_id`, or `None` (logged) when no target was
/// recorded for it.
pub fn resolve(
    targets: &PromptTargets,
    prompt_id: PromptId,
    action: PromptAction,
) -> Option<ActionOutcome> {
    let Some(target) = targets.get(prompt_id) else {
        eprintln!(
            "nguda: action {} for unknown prompt {prompt_id}; dropped",
            action_name(action)
        );
        return None;
    };
    Some(action_msg(target, action))
}

/// Posts each prompt at most once.
struct Consumer {
    notifier: Arc<dyn Notifier>,
    posted: HashSet<PromptId>,
    targets: PromptTargets,
}

impl Consumer {
    fn new(notifier: Arc<dyn Notifier>, targets: PromptTargets) -> Self {
        Self {
            notifier,
            posted: HashSet::new(),
            targets,
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
        let task_title = match (kind, snapshot.overflow.len()) {
            (PromptKind::Overflow, n) if n > 1 => format!("{} and {} more", item.title(), n - 1),
            _ => item.title().to_owned(),
        };
        let note = Notification {
            prompt_id,
            kind,
            task_title,
            actions: actions_for(kind),
        };
        // Recorded before posting, so a click that comes back at once finds it.
        if !note.actions.is_empty() {
            self.targets.record(prompt_id, (item_id, snapshot.rev));
        }
        if let Err(e) = self.notifier.post(&note) {
            // NotifierError messages carry ids and OS errors only.
            eprintln!(
                "nguda: failed to post prompt {prompt_id} ({}) for item {item_id}: {e}",
                kind_name(kind)
            );
        }
    }
}

/// The buttons a prompt kind offers: all four at planned end, none for overflow.
fn actions_for(kind: PromptKind) -> Vec<PromptAction> {
    match kind {
        PromptKind::PlannedEnd => PromptAction::ALL.to_vec(),
        PromptKind::Overflow => Vec::new(),
    }
}

fn kind_name(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::PlannedEnd => "planned_end",
        PromptKind::Overflow => "overflow",
    }
}

fn action_name(action: PromptAction) -> &'static str {
    match action {
        PromptAction::Extend5 => "extend5",
        PromptAction::Extend15 => "extend15",
        PromptAction::ExtendCustom => "extend_custom",
        PromptAction::Switch => "switch",
    }
}

/// Renders effects until the engine stops, recording each actioned prompt's
/// target in `targets`.
pub async fn consume(
    mut effects: mpsc::UnboundedReceiver<Effect>,
    snapshots: watch::Receiver<Snapshot>,
    notifier: Arc<dyn Notifier>,
    targets: PromptTargets,
) {
    let mut consumer = Consumer::new(notifier, targets);
    while let Some(effect) = effects.recv().await {
        // Copy out, so the lock is not held while posting.
        let snapshot = snapshots.borrow().clone();
        consumer.handle(effect, &snapshot);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nguda_adapters::{SqliteStore, SystemClock, UuidV7Ids};
    use nguda_core::{Date, Day, ItemStatus, NotifierError, OrderKey, PlanItem, TimeZone, Uuid};
    use nguda_engine::{EngineDeps, StatusKey};
    use tokio::time::timeout;

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
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
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
                    actions: PromptAction::ALL.to_vec(),
                },
                Notification {
                    prompt_id: prompt(2),
                    kind: PromptKind::PlannedEnd,
                    task_title: "Write".into(),
                    actions: PromptAction::ALL.to_vec(),
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
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
        c.handle(notify(1), &snapshot());
        c.handle(notify(1), &snapshot());
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }

    #[test]
    fn recovered_and_unknown_items_post_nothing() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
        c.handle(Effect::Recovered { item_id: item_id() }, &snapshot());
        c.handle(notify(1), &Snapshot::default());
        assert!(fake.posted.lock().unwrap().is_empty());
    }

    #[test]
    fn withdraw_reaches_the_notifier_once_and_never_re_posts() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
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
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
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
    fn an_overflow_prompt_names_the_first_leftover_and_counts_the_rest() {
        let fake = Arc::new(FakeNotifier::default());
        let mut c = Consumer::new(fake.clone(), PromptTargets::default());
        let overflow = |n: u128| Effect::Notify {
            prompt_id: prompt(n),
            item_id: item_id(),
            kind: PromptKind::Overflow,
        };
        let mut snap = snapshot();
        snap.overflow = vec![item_id()];
        c.handle(overflow(1), &snap);
        snap.overflow = vec![
            item_id(),
            ItemId(Uuid::from_u128(2)),
            ItemId(Uuid::from_u128(3)),
        ];
        c.handle(overflow(2), &snap);
        let titles: Vec<(PromptKind, String)> = fake
            .posted
            .lock()
            .unwrap()
            .iter()
            .map(|n| (n.kind, n.task_title.clone()))
            .collect();
        assert_eq!(
            titles,
            [
                (PromptKind::Overflow, "Write".to_owned()),
                (PromptKind::Overflow, "Write and 2 more".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn consume_posts_until_the_engine_stops() {
        let fake = Arc::new(FakeNotifier::default());
        let (tx, rx) = mpsc::unbounded_channel();
        let (_snap_tx, snaps) = watch::channel(snapshot());
        tx.send(notify(1)).unwrap();
        tx.send(notify(1)).unwrap();
        drop(tx);
        consume(rx, snaps, fake.clone(), PromptTargets::default()).await;
        assert_eq!(fake.posted.lock().unwrap().len(), 1);
    }

    /// The planned-end prompt `n` posted from `snapshot()` (rev 2), with its target
    /// recorded.
    fn posted_targets(n: u128) -> PromptTargets {
        let targets = PromptTargets::default();
        let mut c = Consumer::new(Arc::new(FakeNotifier::default()), targets.clone());
        c.handle(notify(n), &snapshot());
        targets
    }

    #[test]
    fn a_planned_end_prompt_offers_the_four_actions_and_records_its_target() {
        let fake = Arc::new(FakeNotifier::default());
        let targets = PromptTargets::default();
        let mut c = Consumer::new(fake.clone(), targets.clone());
        c.handle(notify(1), &snapshot());
        assert_eq!(
            fake.posted.lock().unwrap()[0].actions,
            [
                PromptAction::Extend5,
                PromptAction::Extend15,
                PromptAction::ExtendCustom,
                PromptAction::Switch
            ]
        );
        assert_eq!(targets.get(prompt(1)), Some((item_id(), 2)));
    }

    #[test]
    fn extend_5_sends_extend_task_by_300_at_the_posted_rev() {
        assert_eq!(
            resolve(&posted_targets(1), prompt(1), PromptAction::Extend5),
            Some(ActionOutcome::Send(EngineMsg::ExtendTask {
                item_id: item_id(),
                secs: 300,
                expected_rev: 2,
            }))
        );
    }

    #[test]
    fn extend_15_sends_extend_task_by_900_at_the_posted_rev() {
        assert_eq!(
            resolve(&posted_targets(1), prompt(1), PromptAction::Extend15),
            Some(ActionOutcome::Send(EngineMsg::ExtendTask {
                item_id: item_id(),
                secs: 900,
                expected_rev: 2,
            }))
        );
    }

    #[test]
    fn switch_sends_switch_at_the_posted_rev() {
        assert_eq!(
            resolve(&posted_targets(1), prompt(1), PromptAction::Switch),
            Some(ActionOutcome::Send(EngineMsg::Switch {
                item_id: item_id(),
                expected_rev: 2,
            }))
        );
    }

    #[test]
    fn custom_focuses_the_extend_field_and_sends_nothing() {
        assert_eq!(
            resolve(&posted_targets(1), prompt(1), PromptAction::ExtendCustom),
            Some(ActionOutcome::FocusExtend { item_id: item_id() })
        );
    }

    #[test]
    fn an_action_for_an_unknown_prompt_is_dropped() {
        for action in PromptAction::ALL {
            assert_eq!(resolve(&posted_targets(1), prompt(2), action), None);
        }
    }

    #[test]
    fn the_target_is_the_posted_snapshot_not_a_later_one() {
        let targets = PromptTargets::default();
        let mut c = Consumer::new(Arc::new(FakeNotifier::default()), targets.clone());
        c.handle(notify(1), &snapshot());
        // A later snapshot (a newer rev) changes nothing already recorded, and a
        // withdrawn prompt keeps its target, so a late click is rejected as stale.
        let later = Snapshot {
            rev: 9,
            ..snapshot()
        };
        c.handle(notify(1), &later);
        c.handle(
            Effect::WithdrawNotification {
                prompt_id: prompt(1),
            },
            &later,
        );
        assert_eq!(targets.get(prompt(1)), Some((item_id(), 2)));
    }

    #[test]
    fn an_overflow_prompt_has_no_actions_and_no_target() {
        let fake = Arc::new(FakeNotifier::default());
        let targets = PromptTargets::default();
        let mut c = Consumer::new(fake.clone(), targets.clone());
        let mut snap = snapshot();
        snap.overflow = vec![item_id()];
        c.handle(
            Effect::Notify {
                prompt_id: prompt(1),
                item_id: item_id(),
                kind: PromptKind::Overflow,
            },
            &snap,
        );
        assert!(fake.posted.lock().unwrap()[0].actions.is_empty());
        assert_eq!(resolve(&targets, prompt(1), PromptAction::Extend5), None);
    }

    /// The Stale click row, on a real engine: a prompt posted at rev r, then a
    /// tray Switch commits, then Extend +5 on the prompt carries r and is rejected.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_click_after_a_later_commit_is_rejected_as_stale() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(dir.path().join("nguda.sqlite3")).unwrap();
        let (engine, _effects) = nguda_engine::spawn(EngineDeps {
            clock: Arc::new(SystemClock),
            store: Arc::new(store),
            ids: Arc::new(UuidV7Ids),
            tz: TimeZone::UTC,
        });
        let wait = Duration::from_secs(10);
        let mut rx = engine.subscribe();

        engine
            .send(EngineMsg::CreateTask {
                title: "Write".into(),
                planned_secs: 1500,
            })
            .unwrap();
        let created = timeout(
            wait,
            rx.wait_for(|s| s.day.as_ref().is_some_and(|d| !d.items().is_empty())),
        )
        .await
        .expect("task created in time")
        .unwrap()
        .clone();
        let a = created.day.as_ref().unwrap().items()[0].id();
        engine
            .send(EngineMsg::Start {
                item_id: a,
                expected_rev: created.rev,
            })
            .unwrap();
        let running = timeout(wait, rx.wait_for(|s| s.running.is_some()))
            .await
            .expect("task started in time")
            .unwrap()
            .clone();

        // The prompt is posted from the running snapshot (rev r).
        let targets = PromptTargets::default();
        let mut c = Consumer::new(Arc::new(FakeNotifier::default()), targets.clone());
        c.handle(
            Effect::Notify {
                prompt_id: prompt(1),
                item_id: a,
                kind: PromptKind::PlannedEnd,
            },
            &running,
        );

        // The tray's Switch commits, so the rev moves past r.
        engine.send(switch_target(a, running.rev)).unwrap();
        let switched = timeout(wait, rx.wait_for(|s| s.rev > running.rev))
            .await
            .expect("switch committed in time")
            .unwrap()
            .clone();
        assert_eq!(
            switched.day.as_ref().unwrap().item(a).unwrap().status(),
            ItemStatus::Done
        );
        assert!(
            switched
                .status
                .iter()
                .all(|e| e.key != StatusKey::CommandRejected)
        );

        // Extend +5 on the old prompt still carries r.
        let Some(ActionOutcome::Send(msg)) = resolve(&targets, prompt(1), PromptAction::Extend5)
        else {
            panic!("Extend +5 sends a message");
        };
        assert_eq!(msg, extend_target(a, 300, running.rev));
        engine.send(msg).unwrap();
        let rejected = timeout(
            wait,
            rx.wait_for(|s| s.status.iter().any(|e| e.key == StatusKey::CommandRejected)),
        )
        .await
        .expect("stale click rejected in time")
        .unwrap()
        .clone();
        // Nothing applied: the item keeps its length and stays done.
        let item = rejected.day.as_ref().unwrap().item(a).unwrap();
        assert_eq!(item.planned_secs(), 1500);
        assert_eq!(item.status(), ItemStatus::Done);
        assert_eq!(rejected.running, switched.running);
    }

    #[test]
    fn custom_focuses_the_extend_field_only_while_its_item_runs() {
        let running = |id: ItemId| Snapshot {
            running: Some(nguda_core::Running {
                item_id: id,
                title: "Write".into(),
                segment_started_at: nguda_core::Timestamp::UNIX_EPOCH,
                accumulated_secs: 0,
            }),
            ..snapshot()
        };
        assert!(focuses_extend(item_id(), &running(item_id())));
        // Stale: a tray Switch moved on to B, so A's banner only shows the window.
        let b = ItemId(Uuid::from_u128(2));
        assert!(!focuses_extend(item_id(), &running(b)));
        assert!(!focuses_extend(item_id(), &snapshot()));
    }
}
