//! A buttonless notifier (AD-10) that shells out to the OS, using only
//! `std::process`.
//!
//! - **macOS:** `osascript` with an `on run argv` script, so every piece of text is
//!   an argument and never part of the script source. It posts from any process,
//!   bundled or not, without touching UserNotifications.
//! - **Linux:** `notify-send`, with `--` before the text so a title starting with
//!   `-` is never read as an option.
//! - **Other OS:** unsupported, reported as a [`NotifierError`].
//!
//! The child is spawned without waiting and reaped on its own thread.

use std::process::{Command, Stdio};

use nguda_core::{Notification, Notifier, NotifierError, PromptId, PromptKind};

const APP_NAME: &str = "nguda";

/// The fixed macOS script. Its arguments follow it: the app name first (a fixed
/// string, so no user text is parsed as an option), then the headline and the task
/// title, read only as `item 2` and `item 3 of argv`.
const MACOS_SCRIPT: [&str; 6] = [
    "-e",
    "on run argv",
    "-e",
    "display notification (item 3 of argv) with title (item 1 of argv) subtitle (item 2 of argv)",
    "-e",
    "end run",
];

/// A program and its arguments, built without running anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Invocation {
    pub(crate) program: &'static str,
    pub(crate) args: Vec<String>,
}

/// The headline shown for a prompt kind.
fn headline(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::PlannedEnd => "Planned time is up",
    }
}

pub(crate) fn osascript(n: &Notification) -> Invocation {
    let mut args: Vec<String> = MACOS_SCRIPT.iter().map(|s| (*s).to_owned()).collect();
    args.extend([
        APP_NAME.to_owned(),
        headline(n.kind).to_owned(),
        n.task_title.clone(),
    ]);
    Invocation {
        program: "osascript",
        args,
    }
}

pub(crate) fn notify_send(n: &Notification) -> Invocation {
    Invocation {
        program: "notify-send",
        args: vec![
            format!("--app-name={APP_NAME}"),
            "--".to_owned(),
            headline(n.kind).to_owned(),
            n.task_title.clone(),
        ],
    }
}

/// The command that renders `n` on this OS.
pub(crate) fn command_for(n: &Notification) -> Result<Invocation, NotifierError> {
    if cfg!(target_os = "macos") {
        Ok(osascript(n))
    } else if cfg!(target_os = "linux") {
        Ok(notify_send(n))
    } else {
        Err(NotifierError::new(format!(
            "prompt {}: notifications are not supported on this OS",
            n.prompt_id
        )))
    }
}

/// Posts each notification as a plain banner with no action buttons.
#[derive(Clone, Copy, Debug, Default)]
pub struct ButtonlessNotifier;

impl Notifier for ButtonlessNotifier {
    fn post(&self, n: &Notification) -> Result<(), NotifierError> {
        let inv = command_for(n)?;
        let mut child = Command::new(inv.program)
            .args(&inv.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                NotifierError::new(format!(
                    "prompt {}: cannot run {}: {e}",
                    n.prompt_id, inv.program
                ))
            })?;
        let (prompt_id, program) = (n.prompt_id, inv.program);
        std::thread::Builder::new()
            .name("nguda-notifier-reap".into())
            .spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                // Ids and program only, never the title (AD-14).
                Ok(status) => {
                    eprintln!("nguda: prompt {prompt_id}: {program} exited with {status}")
                }
                Err(e) => eprintln!("nguda: prompt {prompt_id}: waiting for {program} failed: {e}"),
            })
            .map_err(|e| NotifierError::new(format!("prompt {}: cannot reap: {e}", n.prompt_id)))?;
        Ok(())
    }

    /// A no-op. An `osascript` or `notify-send` banner, once posted, has no handle
    /// to remove it by, so there is nothing to withdraw; the banner times out on its
    /// own. The real notifiers (story 4) remove their banners here.
    fn withdraw(&self, _prompt_id: PromptId) -> Result<(), NotifierError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nguda_core::Uuid;

    use super::*;

    fn note(title: &str) -> Notification {
        Notification {
            prompt_id: PromptId(Uuid::from_u128(7)),
            kind: PromptKind::PlannedEnd,
            task_title: title.into(),
        }
    }

    const HOSTILE: [&str; 4] = [
        "-e",
        "--help",
        r#"say "hi" & do shell script "rm -rf ~""#,
        "it's \\ \"quoted\"",
    ];

    #[test]
    fn osascript_passes_text_only_as_trailing_arguments() {
        for title in ["Write report"].into_iter().chain(HOSTILE) {
            let inv = osascript(&note(title));
            assert_eq!(inv.program, "osascript");
            // The script source is fixed: the task text never becomes part of it.
            assert_eq!(&inv.args[..6], &MACOS_SCRIPT.map(String::from));
            assert_eq!(
                &inv.args[6..],
                &["nguda", "Planned time is up", title].map(String::from),
                "{title}"
            );
        }
    }

    #[test]
    fn notify_send_ends_options_before_the_text() {
        for title in ["Write report"].into_iter().chain(HOSTILE) {
            let inv = notify_send(&note(title));
            assert_eq!(inv.program, "notify-send");
            assert_eq!(
                inv.args,
                ["--app-name=nguda", "--", "Planned time is up", title].map(String::from),
                "{title}"
            );
        }
    }

    #[test]
    fn command_for_picks_this_os() {
        let n = note("t");
        let got = command_for(&n);
        if cfg!(target_os = "macos") {
            assert_eq!(got, Ok(osascript(&n)));
        } else if cfg!(target_os = "linux") {
            assert_eq!(got, Ok(notify_send(&n)));
        } else {
            let err = got.unwrap_err();
            assert!(err.message.contains(&n.prompt_id.to_string()));
        }
    }

    #[test]
    fn buttonless_withdraw_is_a_no_op() {
        assert_eq!(
            ButtonlessNotifier.withdraw(PromptId(Uuid::from_u128(7))),
            Ok(())
        );
    }
}
