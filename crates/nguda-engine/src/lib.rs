//! nguda engine: one actor task that owns live state (AD-2, AD-3).
//!
//! Inputs arrive as [`EngineMsg`] on a channel and are handled one at a time. After
//! each applied message the engine bumps `rev` and publishes a [`Snapshot`] on a
//! `watch` channel. It holds no `Mutex` and runs no per-second tick: rendering the
//! ticking elapsed time is the subscriber's job (AD-11).

use std::fmt;
use std::sync::Arc;

use nguda_core::{Clock, Running};
use tokio::sync::{mpsc, watch};

/// Title of the hard-coded tracer-bullet task.
pub const DEMO_TITLE: &str = "Demo";

/// Every input to the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineMsg {
    /// Start the demo task now, restarting its segment if it is already running.
    StartDemo,
}

/// Live state as published after each applied message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub rev: u64,
    pub running: Option<Running>,
}

/// The engine task has stopped, so the message was not delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EngineClosed;

impl fmt::Display for EngineClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("engine is not running")
    }
}

impl std::error::Error for EngineClosed {}

/// Cheap to clone: a sender for inputs and a receiver for snapshots.
#[derive(Clone, Debug)]
pub struct EngineHandle {
    tx: mpsc::UnboundedSender<EngineMsg>,
    snapshots: watch::Receiver<Snapshot>,
}

impl EngineHandle {
    /// Fire-and-forget: queues `msg` without waiting for the engine (AD-4).
    pub fn send(&self, msg: EngineMsg) -> Result<(), EngineClosed> {
        self.tx.send(msg).map_err(|_| EngineClosed)
    }

    /// A receiver that starts at the latest snapshot.
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.snapshots.clone()
    }
}

/// Starts the engine on the current tokio runtime. Must be called from within one.
pub fn spawn(clock: Arc<dyn Clock>) -> EngineHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (snap_tx, snapshots) = watch::channel(Snapshot::default());
    tokio::spawn(async move {
        let mut state = Snapshot::default();
        while let Some(msg) = rx.recv().await {
            state = apply(&state, msg, clock.as_ref());
            // Every receiver may be gone; the engine keeps serving senders regardless.
            snap_tx.send_replace(state.clone());
        }
    });
    EngineHandle { tx, snapshots }
}

/// Computes the next state from a copy of the current one.
fn apply(state: &Snapshot, msg: EngineMsg, clock: &dyn Clock) -> Snapshot {
    match msg {
        EngineMsg::StartDemo => Snapshot {
            rev: state.rev + 1,
            running: Some(Running {
                title: DEMO_TITLE.to_owned(),
                segment_started_at: clock.now(),
                accumulated_secs: 0,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};

    use nguda_core::Timestamp;

    use super::*;

    struct FakeClock(AtomicI64);

    impl FakeClock {
        fn at(secs: i64) -> Arc<Self> {
            Arc::new(Self(AtomicI64::new(secs)))
        }

        fn set(&self, secs: i64) {
            self.0.store(secs, Ordering::SeqCst);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_second(self.0.load(Ordering::SeqCst)).unwrap()
        }
    }

    async fn next(rx: &mut watch::Receiver<Snapshot>) -> Snapshot {
        rx.changed().await.expect("engine stopped");
        rx.borrow_and_update().clone()
    }

    #[tokio::test]
    async fn starts_with_an_empty_snapshot() {
        let engine = spawn(FakeClock::at(1_000));
        assert_eq!(*engine.subscribe().borrow(), Snapshot::default());
    }

    #[tokio::test]
    async fn start_demo_yields_rev_1_running_demo() {
        let engine = spawn(FakeClock::at(1_000));
        let mut rx = engine.subscribe();
        engine.send(EngineMsg::StartDemo).unwrap();
        let snap = next(&mut rx).await;
        assert_eq!(snap.rev, 1);
        let running = snap.running.expect("demo should be running");
        assert_eq!(running.title, "Demo");
        assert_eq!(
            running.segment_started_at,
            Timestamp::from_second(1_000).unwrap()
        );
        assert_eq!(running.accumulated_secs, 0);
    }

    #[test]
    fn send_after_engine_stopped_returns_engine_closed() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let engine = rt.block_on(async { spawn(FakeClock::at(1_000)) });
        drop(rt); // drops the engine task and its receiver
        assert_eq!(engine.send(EngineMsg::StartDemo), Err(EngineClosed));
    }

    #[tokio::test]
    async fn start_demo_again_restarts_the_segment_and_bumps_rev() {
        let clock = FakeClock::at(1_000);
        let engine = spawn(clock.clone());
        let mut rx = engine.subscribe();
        engine.send(EngineMsg::StartDemo).unwrap();
        next(&mut rx).await;
        clock.set(1_042);
        engine.send(EngineMsg::StartDemo).unwrap();
        let snap = next(&mut rx).await;
        assert_eq!(snap.rev, 2);
        let running = snap.running.unwrap();
        assert_eq!(
            running.segment_started_at,
            Timestamp::from_second(1_042).unwrap()
        );
        assert_eq!(running.accumulated_secs, 0);
    }
}
