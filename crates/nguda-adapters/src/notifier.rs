//! The system notifier (AD-10): renders the engine's prompts with action buttons
//! where the platform allows, and reports each button click back as
//! `(prompt_id, action)` through the callback it was built with. It knows nothing
//! of engine messages or revs: the shell maps an action to a command (AD-1, AD-15).
//!
//! - **macOS** ([`macos`]): UserNotifications, only when the process has a bundle
//!   identifier and the user granted authorization. Otherwise a buttonless
//!   `osascript` banner, so an unbundled dev build never touches UserNotifications.
//! - **Linux** ([`linux`]): `notify-rust` over D-Bus, with the actions only when the
//!   server advertises the `actions` capability, and one waiting thread per
//!   notification.
//! - **Other OS:** unsupported, reported as a [`NotifierError`].
//!
//! Logs and errors carry ids, kinds and action names only, never titles (AD-14).

use std::sync::Arc;

use nguda_core::{Notification, Notifier, NotifierError, PromptAction, PromptId, PromptKind};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

const APP_NAME: &str = "nguda";

/// Called with the prompt and the button the user clicked. Runs on a notifier
/// thread (a waiter thread on Linux, the UserNotifications delegate on macOS), so
/// it must not block.
pub type ActionCallback = Arc<dyn Fn(PromptId, PromptAction) + Send + Sync>;

/// The headline shown for a prompt kind.
fn headline(kind: PromptKind) -> &'static str {
    match kind {
        PromptKind::PlannedEnd => "Planned time is up",
        PromptKind::Overflow => "Plan runs past work-end",
    }
}

/// The fixed identifier of an action, sent to the notification server and read
/// back on a click.
fn action_id(action: PromptAction) -> &'static str {
    match action {
        PromptAction::Extend5 => "extend5",
        PromptAction::Extend15 => "extend15",
        PromptAction::ExtendCustom => "extend-custom",
        PromptAction::Switch => "switch",
    }
}

/// The action an identifier names, or `None` for anything else (a click on the
/// notification body, a close, an unknown id).
fn action_from_id(id: &str) -> Option<PromptAction> {
    PromptAction::ALL.into_iter().find(|a| action_id(*a) == id)
}

/// The button label, matching the tray item of the same action.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn action_label(action: PromptAction) -> &'static str {
    match action {
        PromptAction::Extend5 => "Extend +5 min",
        PromptAction::Extend15 => "Extend +15 min",
        PromptAction::ExtendCustom => "Extend…",
        PromptAction::Switch => "Switch",
    }
}

/// Posts prompts through the OS notification system and reports button clicks.
pub struct SystemNotifier {
    #[cfg(target_os = "linux")]
    backend: linux::Backend,
    #[cfg(target_os = "macos")]
    backend: macos::Backend,
}

impl SystemNotifier {
    /// Builds the notifier. On macOS with a bundle identifier this registers the
    /// planned-end category and asks for authorization once; call it once, early.
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos")),
        allow(unused_variables)
    )]
    pub fn new(on_action: ActionCallback) -> Self {
        Self {
            #[cfg(target_os = "linux")]
            backend: linux::Backend::new(on_action),
            #[cfg(target_os = "macos")]
            backend: macos::Backend::new(on_action),
        }
    }
}

impl Notifier for SystemNotifier {
    fn post(&self, n: &Notification) -> Result<(), NotifierError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self.backend.post(n);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        Err(NotifierError::new(format!(
            "prompt {}: notifications are not supported on this OS",
            n.prompt_id
        )))
    }

    fn withdraw(&self, prompt_id: PromptId) -> Result<(), NotifierError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self.backend.withdraw(prompt_id);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = prompt_id;
            Ok(())
        }
    }
}

/// The macOS fallback: a buttonless banner through `osascript`, which posts from
/// any process, bundled or not, without touching UserNotifications.
#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod osascript {
    use std::process::{Command, Stdio};

    use nguda_core::{Notification, NotifierError};

    use super::{APP_NAME, headline};

    /// The fixed script. Its arguments follow it: the app name first (a fixed
    /// string, so no user text is parsed as an option), then the headline and the
    /// task title, read only as `item 2` and `item 3 of argv`. Every piece of text
    /// is an argument and never part of the script source.
    pub(super) const SCRIPT: [&str; 6] = [
        "-e",
        "on run argv",
        "-e",
        "display notification (item 3 of argv) with title (item 1 of argv) subtitle (item 2 of argv)",
        "-e",
        "end run",
    ];

    /// The arguments for `osascript`, built without running anything.
    pub(super) fn args(n: &Notification) -> Vec<String> {
        let mut args: Vec<String> = SCRIPT.iter().map(|s| (*s).to_owned()).collect();
        args.extend([
            APP_NAME.to_owned(),
            headline(n.kind).to_owned(),
            n.task_title.clone(),
        ]);
        args
    }

    /// Spawns `osascript` without waiting and reaps it on its own thread.
    pub(super) fn post(n: &Notification) -> Result<(), NotifierError> {
        let mut child = Command::new("osascript")
            .args(args(n))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                NotifierError::new(format!("prompt {}: cannot run osascript: {e}", n.prompt_id))
            })?;
        let prompt_id = n.prompt_id;
        std::thread::Builder::new()
            .name("nguda-notifier-reap".into())
            .spawn(move || match child.wait() {
                Ok(status) if status.success() => {}
                // Ids and program only, never the title (AD-14).
                Ok(status) => {
                    eprintln!("nguda: prompt {prompt_id}: osascript exited with {status}")
                }
                Err(e) => eprintln!("nguda: prompt {prompt_id}: waiting for osascript failed: {e}"),
            })
            .map_err(|e| NotifierError::new(format!("prompt {}: cannot reap: {e}", n.prompt_id)))?;
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
            actions: PromptAction::ALL.to_vec(),
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
            let args = osascript::args(&note(title));
            // The script source is fixed: the task text never becomes part of it.
            assert_eq!(&args[..6], &osascript::SCRIPT.map(String::from));
            assert_eq!(
                &args[6..],
                &["nguda", "Planned time is up", title].map(String::from),
                "{title}"
            );
        }
    }

    #[test]
    fn action_ids_are_fixed_strings() {
        assert_eq!(
            PromptAction::ALL.map(action_id),
            ["extend5", "extend15", "extend-custom", "switch"]
        );
    }

    #[test]
    fn action_from_id_inverts_action_id() {
        for action in PromptAction::ALL {
            assert_eq!(
                action_from_id(action_id(action)),
                Some(action),
                "{action:?}"
            );
        }
    }

    #[test]
    fn action_from_id_rejects_anything_else() {
        for id in [
            "",
            "default",
            "__closed",
            "Extend5",
            "extend",
            "switch ",
            "com.apple.UNNotificationDefaultActionIdentifier",
        ] {
            assert_eq!(action_from_id(id), None, "{id:?}");
        }
    }

    #[test]
    fn action_labels_match_the_tray() {
        assert_eq!(
            PromptAction::ALL.map(action_label),
            ["Extend +5 min", "Extend +15 min", "Extend…", "Switch"]
        );
    }
}
