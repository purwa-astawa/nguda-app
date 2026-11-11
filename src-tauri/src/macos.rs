//! macOS-only shell helpers (AD-11).
//!
//! `apply_monospaced_digits` gives the tray's status-bar button a font with
//! monospaced digits, so the ticking title keeps a steady width. Tauri exposes no
//! handle to that button, so the helper finds it in AppKit's private view
//! hierarchy (a window whose class name contains `StatusBarWindow`, holding an
//! `NSButton`). That dependency was proven in an untracked framework spike. If the
//! hierarchy changes, the lookup fails and the title keeps the default font.

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSButton, NSFont, NSFontWeightRegular, NSView};

/// Sets a monospaced-digit system font on the status-bar button, at its current
/// point size. Returns `false`, changing nothing, when called off the main thread
/// or when no status-bar button is found. Run once after the tray is built: the
/// button keeps its font across `setTitle:`, so it never runs per tick.
pub fn apply_monospaced_digits() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    for window in app.windows().iter() {
        if !window
            .class()
            .name()
            .to_string_lossy()
            .contains("StatusBarWindow")
        {
            continue;
        }
        let Some(button) = window.contentView().and_then(|v| find_button(&v)) else {
            continue;
        };
        let size = button.font().map(|f| f.pointSize()).unwrap_or(0.0);
        let size = if size > 0.0 {
            size
        } else {
            NSFont::systemFontSize()
        };
        // SAFETY: `NSFontWeightRegular` is an AppKit-provided extern constant, which
        // objc2 marks unsafe to read; it is initialised before any AppKit call runs.
        let weight = unsafe { NSFontWeightRegular };
        let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, weight);
        button.setFont(Some(&font));
        return true;
    }
    false
}

/// The first `NSButton` in `view` or its subviews, depth first.
fn find_button(view: &NSView) -> Option<Retained<NSButton>> {
    if let Some(b) = view.downcast_ref::<NSButton>() {
        return Some(Retained::from(b));
    }
    view.subviews().iter().find_map(|v| find_button(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_the_main_thread_it_changes_nothing_and_returns_false() {
        // Test bodies run on worker threads, never on the process's main thread.
        let on_worker = std::thread::spawn(apply_monospaced_digits).join().unwrap();
        assert!(!on_worker);
    }
}
