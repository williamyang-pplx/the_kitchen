//! `KITCHEN_HIDDEN=1`: a kitchen nobody sees, for agents that test UI
//! changes. Its window is still drawn every frame (App Nap is held off), so
//! `kitchenctl debug screenshot` works, but it is transparent, ignores the mouse, sits off
//! screen, never takes focus and has no Dock icon.

use std::sync::OnceLock;

pub fn active() -> bool {
    static HIDDEN: OnceLock<bool> = OnceLock::new();
    *HIDDEN.get_or_init(|| std::env::var_os("KITCHEN_HIDDEN").is_some())
}

/// Hide the window. AppKit pulls a window back on screen when it is first
/// shown, so this runs on every frame and moves it again if it came back.
#[cfg(target_os = "macos")]
pub fn hide(window: &impl wry::raw_window_handle::HasWindowHandle) {
    use objc2_app_kit::{NSView, NSWindowCollectionBehavior};
    use objc2_foundation::NSPoint;
    use wry::raw_window_handle::RawWindowHandle;

    stay_awake();
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    // SAFETY: the handle's view is the live content view of our window, on the main thread.
    let view: &NSView = unsafe { h.ns_view.cast().as_ref() };
    let Some(window) = view.window() else { return };
    if window.frame().origin.x <= -10_000.0 {
        return;
    }
    window.setAlphaValue(0.0);
    window.setIgnoresMouseEvents(true);
    window.setExcludedFromWindowsMenu(true);
    window.setCollectionBehavior(NSWindowCollectionBehavior::Transient | NSWindowCollectionBehavior::IgnoresCycle);
    window.setFrameOrigin(NSPoint::new(-30_000.0, 0.0));
}

/// Keep macOS from napping the app (App Nap). A window nobody can see gets
/// no input, and after a few seconds of that macOS stops scheduling its
/// frames: injected keys and screenshots then wait, and timings stretch.
#[cfg(target_os = "macos")]
fn stay_awake() {
    use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
    static ACTIVITY: OnceLock<()> = OnceLock::new();
    ACTIVITY.get_or_init(|| {
        let reason = NSString::from_str("hidden test kitchen");
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(NSActivityOptions::UserInitiatedAllowingIdleSystemSleep, &reason);
        // Held for the life of the process.
        std::mem::forget(token);
    });
}

#[cfg(not(target_os = "macos"))]
pub fn hide(_window: &impl wry::raw_window_handle::HasWindowHandle) {}
