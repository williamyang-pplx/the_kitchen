//! Images for Claude Code. ⌘V with an image (and no text) on the clipboard,
//! or files dropped on the terminal, paste the files' paths: Claude Code
//! attaches a pasted path to a .png/.jpg/.gif/.webp file as an image. It
//! reads that file (and, for its own Ctrl+V, the clipboard) on the host it
//! runs on, so a clipboard image is saved to a file first, and for a devbox
//! tab every file is copied to `~/.kitchen/attachments` there (kept 7 days).
//!
//! egui-winit only pastes text: for ⌘V with no text on the clipboard it
//! sends nothing at all, so an AppKit key monitor notices the ⌘V instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use kitchen_core::host::Host;
use kitchen_core::util::shell_quote;

use crate::engine::Engine;
use crate::terminal::Paster;

/// Dropped files bigger than this are not copied to a devbox.
const MAX_UPLOAD: u64 = 64 << 20;

static CMD_V: AtomicBool = AtomicBool::new(false);

/// Start watching for ⌘V. Call once, on the main thread.
pub fn install() {
    imp::install();
}

/// Whether ⌘V was pressed since the last call.
pub fn take_cmd_v() -> bool {
    CMD_V.swap(false, Ordering::Relaxed)
}

pub enum Source {
    /// The clipboard's image; nothing when it has text (egui pasted that) or no image.
    Clipboard,
    /// A file dropped on the terminal.
    File(PathBuf),
}

/// On a background thread: make `sources` readable on `host`, then paste
/// their paths, space-separated, through `paster`. Failures become toasts.
pub fn deliver(engine: &Arc<Engine>, host: Host, sources: Vec<Source>, paster: Paster) {
    engine.spawn("attach", move |_| {
        let mut paths = Vec::new();
        for source in sources {
            if let Some(path) = place(&host, source)? {
                paths.push(escape(&path));
            }
        }
        if !paths.is_empty() {
            paster.paste(&paths.join(" "));
        }
        Ok(())
    });
}

/// The path on `host` of `source`'s file, if there is one.
fn place(host: &Host, source: Source) -> Result<Option<String>> {
    match source {
        Source::Clipboard => {
            let Some(png) = imp::clipboard_image() else { return Ok(None) };
            let name = format!("clipboard-{}.png", stamp());
            if host.is_local() {
                let dir = std::env::temp_dir().join("kitchen-attachments");
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(name);
                std::fs::write(&path, png)?;
                return Ok(Some(path.display().to_string()));
            }
            upload(host, "$HOME/.kitchen/attachments", &name, &png).map(Some)
        }
        Source::File(path) if host.is_local() => Ok(Some(path.display().to_string())),
        Source::File(path) => {
            let meta = std::fs::metadata(&path).with_context(|| path.display().to_string())?;
            if !meta.is_file() {
                bail!("{} is not a file, so it cannot be copied to {}", path.display(), host.label());
            }
            if meta.len() > MAX_UPLOAD {
                bail!("{} is over {} MB, too big to copy to {}", path.display(), MAX_UPLOAD >> 20, host.label());
            }
            let bytes = std::fs::read(&path).with_context(|| path.display().to_string())?;
            let name = format!("{}-{}", stamp(), file_name(&path));
            upload(host, "$HOME/.kitchen/attachments", &name, &bytes).map(Some)
        }
    }
}

/// Write `bytes` to `dir/name` on `host` (`dir` is expanded by the host's
/// shell) and return the file's path there. Drops week-old files from `dir`.
fn upload(host: &Host, dir: &str, name: &str, bytes: &[u8]) -> Result<String> {
    let script = format!(
        r#"d="{dir}" && mkdir -p "$d" && find "$d" -type f -mtime +7 -delete; f="$d"/{name} && cat > "$f" && printf %s "$f""#,
        name = shell_quote(name)
    );
    let out = host.run_with_stdin(&["sh", "-c", &script], None, Some(bytes))?;
    Ok(out.into_result(&format!("copy {name} to {}", host.label()))?.trim().to_string())
}

/// Milliseconds since the epoch, to keep file names apart.
fn stamp() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// `path`'s file name with everything but `[A-Za-z0-9._-]` made `_`, so the
/// devbox path needs no escaping.
fn file_name(path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect()
}

/// Backslash before anything a shell would split on or expand, as
/// Terminal.app does for dropped files. Claude Code takes the backslashes out.
fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        if !(c.is_alphanumeric() || "/._-+,:@%".contains(c)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ptr::NonNull;
    use std::sync::atomic::Ordering;

    use block2::RcBlock;
    use objc2_app_kit::{
        NSBitmapImageFileType, NSBitmapImageRep, NSEvent, NSEventMask, NSEventModifierFlags, NSPasteboard, NSPasteboardTypePNG,
        NSPasteboardTypeString, NSPasteboardTypeTIFF,
    };
    use objc2_foundation::NSDictionary;

    pub fn install() {
        let chord = NSEventModifierFlags::Command | NSEventModifierFlags::Shift | NSEventModifierFlags::Control | NSEventModifierFlags::Option;
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit hands the monitor a valid event.
            let e = unsafe { event.as_ref() };
            if e.modifierFlags() & chord == NSEventModifierFlags::Command && e.charactersIgnoringModifiers().is_some_and(|c| c.to_string() == "v") {
                super::CMD_V.store(true, Ordering::Relaxed);
            }
            event.as_ptr() // the event goes on to winit
        });
        // SAFETY: the block returns the event it was given. The monitor lives
        // as long as the app.
        std::mem::forget(unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block) });
    }

    /// The clipboard's image as PNG, unless the clipboard also has text.
    pub fn clipboard_image() -> Option<Vec<u8>> {
        objc2::rc::autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            // SAFETY: AppKit's pasteboard type constants are initialized statics.
            let (string, png, tiff) = unsafe { (NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
            if pb.stringForType(string).is_some_and(|s| s.length() > 0) {
                return None;
            }
            if let Some(data) = pb.dataForType(png) {
                return Some(data.to_vec());
            }
            // Some apps copy images as TIFF only.
            let rep = NSBitmapImageRep::imageRepWithData(&*pb.dataForType(tiff)?)?;
            // SAFETY: an empty properties dictionary.
            let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new()) }?;
            Some(data.to_vec())
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn install() {}

    pub fn clipboard_image() -> Option<Vec<u8>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_like_terminal_app() {
        assert_eq!(escape("/tmp/kitchen-attachments/clipboard-1.png"), "/tmp/kitchen-attachments/clipboard-1.png");
        assert_eq!(escape("/Users/w/Screen Shot (2).png"), r"/Users/w/Screen\ Shot\ \(2\).png");
        assert_eq!(escape("/a/it's $x.jpg"), r"/a/it\'s\ \$x.jpg");
    }

    #[test]
    fn devbox_names_need_no_quoting() {
        assert_eq!(file_name(Path::new("/Users/w/Screen Shot 10.00.00\u{202f}AM.png")), "Screen_Shot_10.00.00_AM.png");
        assert_eq!(file_name(Path::new("/x/ü'b.webp")), "__b.webp");
    }

    /// The upload script on this mac: the file lands in `dir`, its path
    /// comes back, and files over a week old are dropped.
    #[test]
    fn upload_writes_the_file_and_prunes_old_ones() {
        let dir = std::env::temp_dir().join(format!("kitchen-attach-test-{}", stamp()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old.png");
        std::fs::write(&old, b"x").unwrap();
        assert!(std::process::Command::new("touch").args(["-t", "202001010000"]).arg(&old).status().unwrap().success());

        let path = upload(&Host::Local, dir.to_str().unwrap(), "it's 1.png", b"\x89PNG data").unwrap();
        assert_eq!(path, dir.join("it's 1.png").to_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG data");
        assert!(!old.exists(), "a week-old file is pruned");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
