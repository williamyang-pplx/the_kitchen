//! macOS banners. Inside The Kitchen.app they go through
//! UNUserNotificationCenter, so they carry the app's icon and a click brings
//! the kitchen forward on the tab that raised them. `cargo run` builds have no
//! bundle (UNUserNotificationCenter throws without one), so they fall back to
//! osascript, which shows the banner as Script Editor's.

use std::sync::{Arc, OnceLock, Weak};

use crate::engine::Engine;

static ENGINE: OnceLock<Weak<Engine>> = OnceLock::new();

/// Take over notification clicks and ask for permission (macOS asks once).
/// Does nothing outside an app bundle.
pub fn install(engine: &Arc<Engine>) {
    if ENGINE.set(Arc::downgrade(engine)).is_ok() && bundled() {
        imp::install();
    }
}

/// Raise a banner for notification `id`; clicking it selects `tab_id`, if given.
pub fn post(id: i64, title: &str, body: &str, tab_id: Option<&str>) {
    if ENGINE.get().is_some() && bundled() {
        imp::post(id, title, body, tab_id.unwrap_or(""));
    } else {
        let title = format!("kitchen: {title}");
        let script = format!("display notification {} with title {}", applescript_string(body), applescript_string(&title));
        let _ = std::process::Command::new("osascript").arg("-e").arg(script).spawn();
    }
}

/// Banner click: bring the window forward and select the tab.
fn clicked(tab_id: &str) {
    let Some(engine) = ENGINE.get().and_then(Weak::upgrade) else { return };
    if !tab_id.is_empty() {
        engine.select_tab(tab_id);
    }
    engine.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
}

#[cfg(target_os = "macos")]
fn bundled() -> bool {
    objc2_foundation::NSBundle::mainBundle().bundleIdentifier().is_some()
}

#[cfg(not(target_os = "macos"))]
fn bundled() -> bool {
    false
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
mod imp {
    use block2::{DynBlock, RcBlock};
    use objc2::rc::Retained;
    use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{ClassType, define_class, msg_send};
    use objc2_foundation::{NSError, NSString};
    use objc2_user_notifications::{
        UNAuthorizationOptions, UNMutableNotificationContent, UNNotification, UNNotificationPresentationOptions,
        UNNotificationRequest, UNNotificationResponse, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
    };

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "KitchenNotificationDelegate"]
        struct Delegate;

        unsafe impl NSObjectProtocol for Delegate {}

        unsafe impl UNUserNotificationCenterDelegate for Delegate {
            // Show banners while the kitchen is frontmost too (macOS hides them by default).
            #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
            fn will_present(
                &self,
                _center: &UNUserNotificationCenter,
                _notification: &UNNotification,
                handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
            ) {
                handler.call((UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,));
            }

            #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
            fn did_receive(
                &self,
                _center: &UNUserNotificationCenter,
                response: &UNNotificationResponse,
                handler: &DynBlock<dyn Fn()>,
            ) {
                // The tab id rides in the thread identifier (which also groups banners per tab).
                super::clicked(&response.notification().request().content().threadIdentifier().to_string());
                handler.call(());
            }
        }
    );

    pub fn install() {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        let delegate: Retained<Delegate> = unsafe { msg_send![Delegate::class(), new] };
        center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        // The center holds its delegate weakly; this one lives as long as the app.
        std::mem::forget(delegate);
        let done = RcBlock::new(|granted: Bool, err: *mut NSError| {
            if !granted.as_bool() {
                let why = unsafe { err.as_ref() }.map(|e| e.localizedDescription().to_string());
                log::warn!("notifications not allowed: {}", why.as_deref().unwrap_or("denied in System Settings"));
            }
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
            &done,
        );
    }

    pub fn post(id: i64, title: &str, body: &str, tab_id: &str) {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setThreadIdentifier(&NSString::from_str(tab_id));
        let id = NSString::from_str(&format!("kitchen-{id}"));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        let done = RcBlock::new(|err: *mut NSError| {
            if let Some(e) = unsafe { err.as_ref() } {
                log::warn!("notification failed: {}", e.localizedDescription());
            }
        });
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn install() {}
    pub fn post(_id: i64, _title: &str, _body: &str, _tab_id: &str) {}
}
