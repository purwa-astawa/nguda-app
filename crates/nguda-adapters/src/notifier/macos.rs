//! macOS: UserNotifications (`objc2-user-notifications`), or `osascript`.
//!
//! UserNotifications crashes a process that has no bundle, so it is used only when
//! `NSBundle.mainBundle` has a bundle identifier and lives in an `.app`. Without
//! one (`npm run tauri dev`, tests) every prompt goes through the buttonless
//! `osascript` fallback and UserNotifications is never called.
//!
//! With a bundle, construction registers one category, `planned-end`, carrying the
//! four actions, installs a delegate, and asks for authorization once. Until the
//! user grants it (still pending, denied, or failed) prompts fall back to
//! `osascript`; a refusal is logged once. A notification with actions uses the
//! category, one without posts plainly. The request identifier is the prompt id,
//! so withdraw removes the delivered (or pending) notification by it, and the
//! delegate reads it back on a click to report `(prompt_id, action)`. The delegate
//! also lets banners show while the app is frontmost.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use block2::RcBlock;
use nguda_core::{Notification, NotifierError, PromptAction, PromptId, Uuid};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSArray, NSBundle, NSError, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationAction,
    UNNotificationActionOptions, UNNotificationCategory, UNNotificationCategoryOptions,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};

use super::{ActionCallback, action_from_id, action_id, action_label, headline, osascript};

const CATEGORY: &str = "planned-end";

const PENDING: u8 = 0;
const GRANTED: u8 = 1;
const DENIED: u8 = 2;

pub(super) struct Backend {
    /// The authorization state, or `None` without a bundle: `osascript` only.
    auth: Option<Arc<AtomicU8>>,
}

impl Backend {
    pub(super) fn new(on_action: ActionCallback) -> Self {
        if !has_bundle_identifier() {
            return Self { auth: None };
        }
        autoreleasepool(|_| {
            let center = UNUserNotificationCenter::currentNotificationCenter();
            let delegate = Delegate::new(on_action);
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // The center holds its delegate weakly; this one lives as long as the app.
            std::mem::forget(delegate);
            center.setNotificationCategories(&planned_end_category());

            let auth = Arc::new(AtomicU8::new(PENDING));
            let state = auth.clone();
            let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
                if granted.as_bool() {
                    state.store(GRANTED, Ordering::SeqCst);
                    return;
                }
                state.store(DENIED, Ordering::SeqCst);
                // SAFETY: UserNotifications passes either null or a valid NSError
                // that outlives this call.
                match unsafe { error.as_ref() } {
                    Some(e) => eprintln!(
                        "nguda: notification authorization failed ({} {}); using osascript",
                        e.domain(),
                        e.code()
                    ),
                    None => {
                        eprintln!("nguda: notification authorization denied; using osascript")
                    }
                }
            });
            center.requestAuthorizationWithOptions_completionHandler(
                UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
                &handler,
            );
            Self { auth: Some(auth) }
        })
    }

    fn authorized(&self) -> bool {
        self.auth
            .as_ref()
            .is_some_and(|a| a.load(Ordering::SeqCst) == GRANTED)
    }

    pub(super) fn post(&self, n: &Notification) -> Result<(), NotifierError> {
        if !self.authorized() {
            return osascript::post(n);
        }
        autoreleasepool(|_| {
            let content = UNMutableNotificationContent::new();
            content.setTitle(&NSString::from_str(headline(n.kind)));
            content.setBody(&NSString::from_str(&n.task_title));
            if !n.actions.is_empty() {
                content.setCategoryIdentifier(&NSString::from_str(CATEGORY));
            }
            let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
                &NSString::from_str(&n.prompt_id.to_string()),
                &content,
                None,
            );
            let fallback = n.clone();
            let done = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: null or a valid NSError that outlives this call.
                let Some(e) = (unsafe { error.as_ref() }) else {
                    return;
                };
                eprintln!(
                    "nguda: prompt {}: UserNotifications refused it ({} {}); using osascript",
                    fallback.prompt_id,
                    e.domain(),
                    e.code()
                );
                if let Err(e) = osascript::post(&fallback) {
                    eprintln!("nguda: {e}");
                }
            });
            UNUserNotificationCenter::currentNotificationCenter()
                .addNotificationRequest_withCompletionHandler(&request, Some(&done));
        });
        Ok(())
    }

    /// Removes the delivered or pending notification `prompt_id`. An `osascript`
    /// banner has no handle, so without UserNotifications this is a no-op.
    pub(super) fn withdraw(&self, prompt_id: PromptId) -> Result<(), NotifierError> {
        if self.auth.is_none() {
            return Ok(());
        }
        autoreleasepool(|_| {
            let ids = NSArray::from_retained_slice(&[NSString::from_str(&prompt_id.to_string())]);
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.removeDeliveredNotificationsWithIdentifiers(&ids);
            center.removePendingNotificationRequestsWithIdentifiers(&ids);
        });
        Ok(())
    }
}

/// Whether this process runs from an `.app` bundle with an identifier, the only
/// case where UserNotifications is safe to call.
fn has_bundle_identifier() -> bool {
    autoreleasepool(|_| {
        let bundle = NSBundle::mainBundle();
        bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
    })
}

/// The `planned-end` category with the four actions. Extend… brings the app
/// forward, since it opens the window; the others act in the background.
fn planned_end_category() -> Retained<NSSet<UNNotificationCategory>> {
    let actions: Vec<Retained<UNNotificationAction>> = PromptAction::ALL
        .into_iter()
        .map(|a| {
            let options = match a {
                PromptAction::ExtendCustom => UNNotificationActionOptions::Foreground,
                _ => UNNotificationActionOptions::empty(),
            };
            UNNotificationAction::actionWithIdentifier_title_options(
                &NSString::from_str(action_id(a)),
                &NSString::from_str(action_label(a)),
                options,
            )
        })
        .collect();
    let category = UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
        &NSString::from_str(CATEGORY),
        &NSArray::from_retained_slice(&actions),
        &NSArray::new(),
        UNNotificationCategoryOptions::empty(),
    );
    NSSet::from_retained_slice(&[category])
}

struct DelegateIvars {
    on_action: ActionCallback,
}

define_class!(
    /// Reports action clicks and shows banners while the app is frontmost.
    #[unsafe(super(NSObject))]
    #[name = "NgudaNotificationDelegate"]
    #[ivars = DelegateIvars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &block2::DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &block2::DynBlock<dyn Fn()>,
        ) {
            let action = response.actionIdentifier().to_string();
            let identifier = response.notification().request().identifier().to_string();
            match (Uuid::parse_str(&identifier), action_from_id(&action)) {
                (Ok(uuid), Some(action)) => (self.ivars().on_action)(PromptId(uuid), action),
                // A click on the banner body or a dismissal: no action.
                (Ok(_), None) => {}
                (Err(_), _) => eprintln!("nguda: notification response without a prompt id"),
            }
            completion_handler.call(());
        }
    }
);

impl Delegate {
    fn new(on_action: ActionCallback) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { on_action });
        // SAFETY: NSObject's designated initializer, on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test binary is unbundled, like `npm run tauri dev`: the notifier must stay
    /// on `osascript` and never reach UserNotifications.
    #[test]
    fn an_unbundled_process_never_uses_user_notifications() {
        assert!(!has_bundle_identifier());
        let backend = Backend::new(Arc::new(|_, _| {}));
        assert!(backend.auth.is_none());
        assert!(!backend.authorized());
        assert_eq!(
            backend.withdraw(PromptId(Uuid::from_u128(7))),
            Ok(()),
            "withdraw is a no-op without UserNotifications"
        );
    }
}
