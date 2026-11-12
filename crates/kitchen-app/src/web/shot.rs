//! Off-screen WebKit pages that lay out HTML and capture it as images. A
//! few WKWebViews sit outside the window's content area; each takes a job
//! (HTML, width, look), reports the laid-out height and links, is sized to
//! that height and snapshotted in tiles, which become egui textures. Lives
//! on the main thread (WebKit's), driven once a frame by [`pump`].

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use egui::{ColorImage, Pos2, Rect, TextureHandle, Vec2};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSBitmapFormat, NSBitmapImageRep, NSImage};
use objc2_foundation::{NSError, NSNumber, NSPoint, NSRect, NSSize};
use objc2_web_kit::WKSnapshotConfiguration;
use wry::dpi::{LogicalPosition, LogicalSize};
use wry::raw_window_handle::HasWindowHandle;
use wry::{WebView, WebViewBuilder, WebViewExtMacOS};

use super::Look;

/// Pages laid out at once.
const WORKERS: usize = 3;
/// Tallest snapshot, in points: taller pages are captured in tiles.
const TILE: f32 = 2048.0;
/// Tallest page captured, in points.
const MAX_HEIGHT: f32 = 60_000.0;
/// A page that hasn't answered by then is given up (and asked for again).
const TIMEOUT: Duration = Duration::from_secs(8);
/// A job nobody has drawn for this long is dropped before it starts.
const STALE: Duration = Duration::from_secs(2);
/// Images not drawn for this long are freed.
const KEEP: Duration = Duration::from_secs(30);
/// Where the pages sit: left of the window, out of sight.
const PARK_X: f64 = -30_000.0;

/// A captured page: its tiles, size and links, in points from its top-left.
pub struct Shot {
    pub tiles: Vec<(TextureHandle, Rect)>,
    pub size: Vec2,
    pub links: Vec<(Rect, String)>,
}

/// The page every worker loads once: GitHub's CSS and a script that takes
/// content, waits for its layout (fonts, images) and reports back.
const SHELL: &str = concat!(
    "<!doctype html><html><head><meta charset=\"utf-8\"><style>",
    include_str!("../../assets/markdown/github-markdown-dark.css"),
    r#"
html, body { margin: 0; padding: 0; overflow: hidden; background: transparent; }
.markdown-body { box-sizing: border-box; padding: 0; background: transparent; font-size: var(--kitchen-size); }
.markdown-body.dim { opacity: 0.6; }
.markdown-body > :first-child { margin-top: 0 !important; }
.markdown-body > :last-child { margin-bottom: 0 !important; }
.kitchen-reply { margin: 14px 0 6px; padding-top: 10px; border-top: 1px solid #3b4261; color: #737aa2; font-size: 0.85em; }
</style></head><body><article class="markdown-body" id="c"></article><script>
function kitchenReport(key) {
  const c = document.getElementById('c');
  const links = [];
  for (const a of c.querySelectorAll('a[href]')) for (const r of a.getClientRects()) links.push([r.left, r.top, r.width, r.height, a.href]);
  window.ipc.postMessage(JSON.stringify({ key, h: Math.ceil(c.getBoundingClientRect().height), links }));
}
window.kitchenSet = function (key, html, size, dim) {
  document.documentElement.style.setProperty('--kitchen-size', size + 'px');
  const c = document.getElementById('c');
  c.className = 'markdown-body' + (dim ? ' dim' : '');
  c.innerHTML = html;
  const images = [...c.querySelectorAll('img')].filter(i => !i.complete).map(i => new Promise(r => { i.onload = i.onerror = r; }));
  const timeout = new Promise(r => setTimeout(r, 2000));
  Promise.race([Promise.all([document.fonts.ready, ...images]), timeout]).then(() => requestAnimationFrame(() => requestAnimationFrame(() => kitchenReport(key))));
};
window.ipc.postMessage(JSON.stringify({ ready: true }));
</script></body></html>"#
);

struct Job {
    key: u64,
    slot: u64,
    width: f32,
    html: String,
    look: Look,
}

enum Stage {
    /// Content sent; waiting for its height.
    Laying,
    /// Sized to its height; waiting a frame or two for WebKit to paint it.
    Sized { height: f32, links: Vec<(Rect, String)>, frames: u32 },
    /// Tiles requested; waiting for them.
    Shooting { height: f32, links: Vec<(Rect, String)>, tiles: Vec<Option<ColorImage>>, left: usize },
}

struct Busy {
    job: Job,
    stage: Stage,
    since: Instant,
}

struct Worker {
    view: WebView,
    ready: bool,
    busy: Option<Busy>,
}

/// What the pages and WebKit's callbacks send back.
enum Msg {
    Ready(usize),
    Laid { worker: usize, key: u64, height: f32, links: Vec<(Rect, String)> },
    Tile { worker: usize, key: u64, index: usize, image: Option<ColorImage> },
}

#[derive(Default)]
struct Snapshots {
    done: HashMap<u64, (Rc<Shot>, Instant)>,
    /// Each slot's latest shot: shown while a newer one is made.
    by_slot: HashMap<u64, u64>,
    queue: VecDeque<Job>,
    /// Keys queued or being made.
    pending: HashSet<u64>,
    /// When each key was last asked for.
    wanted: HashMap<u64, Instant>,
    workers: Vec<Worker>,
    inbox: Rc<RefCell<Vec<Msg>>>,
    /// The window can't host pages (the first failure is logged, once).
    broken: bool,
}

thread_local! {
    static SNAPSHOTS: RefCell<Snapshots> = RefCell::new(Snapshots::default());
}

/// The shot for `key`, or while it is made the slot's last one (None at
/// first). `html` is called only when the key has to be made.
pub fn get(key: u64, slot: u64, width: f32, look: &Look, html: impl FnOnce() -> String) -> Option<Rc<Shot>> {
    SNAPSHOTS.with(|s| {
        let mut s = s.borrow_mut();
        let now = Instant::now();
        s.wanted.insert(key, now);
        if let Some((shot, used)) = s.done.get_mut(&key) {
            *used = now;
            let shot = shot.clone();
            s.by_slot.insert(slot, key);
            return Some(shot);
        }
        if !s.broken && s.pending.insert(key) {
            s.queue.push_back(Job { key, slot, width, html: html(), look: look.clone() });
        }
        let last = *s.by_slot.get(&slot)?;
        s.done.get_mut(&last).map(|(shot, used)| {
            *used = now;
            shot.clone()
        })
    })
}

/// One frame's work: start pages, hand out jobs, collect results.
pub fn pump(window: &impl HasWindowHandle, ctx: &egui::Context) {
    SNAPSHOTS.with(|s| s.borrow_mut().pump(window, ctx));
}

impl Snapshots {
    fn pump(&mut self, window: &impl HasWindowHandle, ctx: &egui::Context) {
        if self.broken {
            return;
        }
        if self.workers.is_empty() && !self.queue.is_empty() {
            for i in 0..WORKERS {
                match self.start_worker(i, window) {
                    Ok(w) => self.workers.push(w),
                    Err(e) => {
                        log::warn!(target: "kitchen::web", "can't start an off-screen page: {e}");
                        self.broken = true;
                        self.queue.clear();
                        self.pending.clear();
                        return;
                    }
                }
            }
        }
        let msgs = std::mem::take(&mut *self.inbox.borrow_mut());
        for msg in msgs {
            self.receive(msg, ctx);
        }
        let now = Instant::now();
        for i in 0..self.workers.len() {
            self.step(i, now);
        }
        // Free images nobody drew lately.
        let done = &mut self.done;
        done.retain(|_, (_, used)| now.duration_since(*used) < KEEP);
        self.by_slot.retain(|_, key| done.contains_key(key));
        self.wanted.retain(|_, at| now.duration_since(*at) < KEEP);
        if !self.queue.is_empty() || self.workers.iter().any(|w| w.busy.is_some()) {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn start_worker(&self, index: usize, window: &impl HasWindowHandle) -> wry::Result<Worker> {
        let inbox = self.inbox.clone();
        let view = WebViewBuilder::new()
            .with_html(SHELL)
            .with_transparent(true)
            .with_focused(false)
            .with_bounds(wry::Rect { position: LogicalPosition::new(PARK_X, 0.0).into(), size: LogicalSize::new(600.0, 600.0).into() })
            .with_ipc_handler(move |req: wry::http::Request<String>| {
                if let Some(msg) = parse(index, req.body()) {
                    inbox.borrow_mut().push(msg);
                }
            })
            .build_as_child(window)?;
        Ok(Worker { view, ready: false, busy: None })
    }

    fn receive(&mut self, msg: Msg, ctx: &egui::Context) {
        match msg {
            Msg::Ready(i) => {
                if let Some(w) = self.workers.get_mut(i) {
                    w.ready = true;
                }
            }
            Msg::Laid { worker, key, height, links } => {
                let Some(w) = self.workers.get_mut(worker) else { return };
                let Some(busy) = w.busy.as_mut().filter(|b| b.job.key == key && matches!(b.stage, Stage::Laying)) else { return };
                let height = height.clamp(1.0, MAX_HEIGHT);
                let _ = w.view.set_bounds(bounds(busy.job.width, height));
                busy.stage = Stage::Sized { height, links, frames: 0 };
                busy.since = Instant::now();
            }
            Msg::Tile { worker, key, index, image } => {
                let Some(w) = self.workers.get_mut(worker) else { return };
                let Some(busy) = w.busy.as_mut().filter(|b| b.job.key == key) else { return };
                let Stage::Shooting { tiles, left, .. } = &mut busy.stage else { return };
                if let Some(t) = tiles.get_mut(index).filter(|t| t.is_none()) {
                    *t = image.or_else(|| Some(ColorImage::new([1, 1], vec![egui::Color32::TRANSPARENT])));
                    *left -= 1;
                }
                if *left > 0 {
                    return;
                }
                let Some(Busy { job, stage: Stage::Shooting { height, links, tiles, .. }, .. }) = w.busy.take() else { return };
                let mut out = Vec::new();
                for (i, image) in tiles.into_iter().enumerate() {
                    let top = i as f32 * TILE;
                    let rect = Rect::from_min_size(Pos2::new(0.0, top), Vec2::new(job.width, TILE.min(height - top)));
                    let texture = ctx.load_texture(format!("markdown-{:x}-{i}", job.key), image.expect("every tile arrived"), egui::TextureOptions::LINEAR);
                    out.push((texture, rect));
                }
                let shot = Rc::new(Shot { tiles: out, size: Vec2::new(job.width, height), links });
                self.done.insert(job.key, (shot, Instant::now()));
                self.by_slot.insert(job.slot, job.key);
                self.pending.remove(&job.key);
                ctx.request_repaint();
            }
        }
    }

    /// Moves worker `i` on: a new job, the snapshot, or giving up.
    fn step(&mut self, i: usize, now: Instant) {
        if !self.workers[i].ready {
            return;
        }
        if let Some(busy) = &self.workers[i].busy {
            if now.duration_since(busy.since) > TIMEOUT {
                log::debug!(target: "kitchen::web", "page {:x} timed out", busy.job.key);
                let key = busy.job.key;
                self.workers[i].busy = None;
                self.pending.remove(&key);
            }
        }
        let w = &mut self.workers[i];
        match &mut w.busy {
            None => {
                while let Some(job) = self.queue.pop_front() {
                    // Nobody draws it any more (scrolled away, text changed).
                    if self.wanted.get(&job.key).is_none_or(|at| now.duration_since(*at) > STALE) {
                        self.pending.remove(&job.key);
                        continue;
                    }
                    let _ = w.view.set_bounds(bounds(job.width, 600.0));
                    let look = &job.look;
                    let js = format!(
                        "kitchenSet({}, {}, {}, {})",
                        serde_json::to_string(&job.key.to_string()).expect("string"),
                        serde_json::to_string(&job.html).expect("string"),
                        look.size,
                        look.dim
                    );
                    if w.view.evaluate_script(&js).is_ok() {
                        w.busy = Some(Busy { job, stage: Stage::Laying, since: now });
                    } else {
                        self.pending.remove(&job.key);
                    }
                    break;
                }
            }
            Some(busy) => {
                // WebKit paints a resized page a frame or two later.
                let Stage::Sized { height, links, frames } = &mut busy.stage else { return };
                *frames += 1;
                if *frames < 3 {
                    return;
                }
                let (height, links) = (*height, std::mem::take(links));
                let count = (height / TILE).ceil().max(1.0) as usize;
                busy.stage = Stage::Shooting { height, links, tiles: (0..count).map(|_| None).collect(), left: count };
                busy.since = now;
                for index in 0..count {
                    let top = index as f32 * TILE;
                    let rect = Rect::from_min_size(Pos2::new(0.0, top), Vec2::new(busy.job.width, TILE.min(height - top)));
                    snapshot(&w.view, rect, i, busy.job.key, index, self.inbox.clone());
                }
            }
        }
    }
}

fn bounds(width: f32, height: f32) -> wry::Rect {
    wry::Rect { position: LogicalPosition::new(PARK_X, 0.0).into(), size: LogicalSize::new(width as f64, height as f64).into() }
}

/// A page's message: `{ready}` or `{key, h, links}`.
fn parse(worker: usize, body: &str) -> Option<Msg> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("ready").is_some() {
        return Some(Msg::Ready(worker));
    }
    let key = v.get("key")?.as_str()?.parse().ok()?;
    let height = v.get("h")?.as_f64()? as f32;
    let links = v
        .get("links")?
        .as_array()?
        .iter()
        .filter_map(|l| {
            let n = |i: usize| l.get(i).and_then(|x| x.as_f64()).map(|x| x as f32);
            let href = l.get(4)?.as_str()?.to_string();
            Some((Rect::from_min_size(Pos2::new(n(0)?, n(1)?), Vec2::new(n(2)?, n(3)?)), href))
        })
        .collect();
    Some(Msg::Laid { worker, key, height, links })
}

/// Asks WebKit for the image of `rect` (points, page coordinates); the
/// result lands in `inbox` as a tile.
fn snapshot(view: &WebView, rect: Rect, worker: usize, key: u64, index: usize, inbox: Rc<RefCell<Vec<Msg>>>) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let webview = view.webview();
    let config = unsafe { WKSnapshotConfiguration::new(mtm) };
    unsafe {
        config.setRect(NSRect::new(NSPoint::new(rect.left() as f64, rect.top() as f64), NSSize::new(rect.width() as f64, rect.height() as f64)));
        config.setSnapshotWidth(Some(&NSNumber::new_f64(rect.width() as f64)));
        config.setAfterScreenUpdates(true);
    }
    let block = block2::RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
        let image = unsafe { image.as_ref() }.and_then(rgba);
        inbox.borrow_mut().push(Msg::Tile { worker, key, index, image });
    });
    unsafe { webview.takeSnapshotWithConfiguration_completionHandler(Some(&config), &block) };
}

/// An NSImage's pixels (8-bit RGB or RGBA, as its bitmap holds them).
fn rgba(image: &NSImage) -> Option<ColorImage> {
    let tiff = image.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let (w, h) = (rep.pixelsWide() as usize, rep.pixelsHigh() as usize);
    let (spp, bps, row) = (rep.samplesPerPixel() as usize, rep.bitsPerSample() as usize, rep.bytesPerRow() as usize);
    let format = rep.bitmapFormat();
    let data = rep.bitmapData();
    if data.is_null() || bps != 8 || !(spp == 3 || spp == 4) || rep.isPlanar() || w == 0 || h == 0 {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data, row * h) };
    let alpha_first = format.contains(NSBitmapFormat::AlphaFirst);
    let mut pixels = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for px in bytes[y * row..y * row + w * spp].chunks_exact(spp) {
            match (spp, alpha_first) {
                (3, _) => pixels.extend_from_slice(&[px[0], px[1], px[2], 255]),
                (_, true) => pixels.extend_from_slice(&[px[1], px[2], px[3], px[0]]),
                _ => pixels.extend_from_slice(px),
            }
        }
    }
    Some(if format.contains(NSBitmapFormat::AlphaNonpremultiplied) {
        ColorImage::from_rgba_unmultiplied([w, h], &pixels)
    } else {
        ColorImage::from_rgba_premultiplied([w, h], &pixels)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_ready_and_layout_messages() {
        assert!(matches!(parse(2, r#"{"ready":true}"#), Some(Msg::Ready(2))));
        let laid = parse(1, r#"{"key":"18446744073709551615","h":120,"links":[[4,10,30,16,"https://x.y/"],[1,2]]}"#);
        let Some(Msg::Laid { worker, key, height, links }) = laid else { panic!("not a layout message") };
        assert_eq!((worker, key, height), (1, u64::MAX, 120.0), "keys pass through JS as strings");
        assert_eq!(links, vec![(Rect::from_min_size(Pos2::new(4.0, 10.0), Vec2::new(30.0, 16.0)), "https://x.y/".to_string())], "a malformed link is skipped");
        assert!(parse(0, "not json").is_none());
    }
}
