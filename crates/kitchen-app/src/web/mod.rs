//! Markdown drawn as GitHub draws it: GitHub's HTML (comrak's until it
//! arrives, or when it can't), GitHub's CSS, laid out by WebKit in an
//! off-screen page and drawn by egui as an image. Cards (review threads,
//! ⌘⇧A answers, plan comments) and rendered documents (⌘⇧V, ⌘⇧D) use it.

pub mod html;
#[cfg(target_os = "macos")]
mod shot;

use std::hash::{Hash, Hasher};

use egui::{Color32, Painter, Pos2, Rect};

use crate::theme;

/// How rendered Markdown looks: its text size (CSS pixels, which are
/// points) and whether it is faded. It is drawn without a background, over
/// whatever it sits on.
#[derive(Debug, Clone, PartialEq)]
pub struct Look {
    pub size: f32,
    pub dim: bool,
}

impl Hash for Look {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (self.size.to_bits(), self.dim).hash(h);
    }
}

/// One piece of what a card shows: a reply's header line, then Markdown.
pub struct Part<'a> {
    pub header: Option<&'a str>,
    pub markdown: &'a str,
}

/// Draws `parts` at `at`, `width` wide, with `painter` (with `paint`;
/// without, only measures) and returns the height. Links land in `links` in screen
/// coordinates. Until WebKit's image is ready the source text stands in,
/// so the card has a size.
#[allow(clippy::too_many_arguments)]
pub fn show(painter: &Painter, paint: bool, parts: &[Part], repo: Option<&str>, look: &Look, at: Pos2, width: f32, links: &mut Vec<(Rect, String)>) -> f32 {
    let width = width.max(40.0).round();
    let mut slot = std::collections::hash_map::DefaultHasher::new();
    let mut key = std::collections::hash_map::DefaultHasher::new();
    let htmls: Vec<_> = parts.iter().map(|p| html::get(p.markdown, repo)).collect();
    for (p, h) in parts.iter().zip(&htmls) {
        (p.header, p.markdown).hash(&mut slot);
        (p.header, &**h).hash(&mut key);
    }
    for h in [&mut slot, &mut key] {
        (repo, width.to_bits(), look).hash(h);
    }
    let page = || {
        let mut out = String::new();
        for (p, h) in parts.iter().zip(&htmls) {
            if let Some(header) = p.header {
                out.push_str(&format!("<div class=\"kitchen-reply\">{}</div>", html::escape(header)));
            }
            out.push_str(h);
        }
        out
    };
    #[cfg(target_os = "macos")]
    if let Some(s) = shot::get(key.finish(), slot.finish(), width, look, page) {
        if paint {
            for (texture, rect) in &s.tiles {
                let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
                painter.image(texture.id(), rect.translate(at.to_vec2()), uv, Color32::WHITE);
            }
        }
        links.extend(s.links.iter().map(|(r, href)| (r.translate(at.to_vec2()), href.clone())));
        return s.size.y;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = page;
    placeholder(painter, paint, parts, look, at, width)
}

/// The source text, faded, in place of an image that isn't ready.
fn placeholder(painter: &Painter, paint: bool, parts: &[Part], look: &Look, at: Pos2, width: f32) -> f32 {
    let text: Vec<&str> = parts.iter().map(|p| p.markdown.trim()).collect();
    let galley = painter.layout(text.join("\n\n"), theme::ui_font(look.size - 1.0), theme::SUBTLE, width);
    let h = galley.size().y;
    if paint {
        painter.galley(at, galley, theme::SUBTLE);
    }
    h
}

/// Drives the off-screen pages: call once a frame, after drawing, with the
/// app's window. Also lets GitHub's renderings ask for a repaint.
pub fn pump(window: &impl wry::raw_window_handle::HasWindowHandle, ctx: &egui::Context) {
    html::set_repaint(ctx);
    #[cfg(target_os = "macos")]
    shot::pump(window, ctx);
    #[cfg(not(target_os = "macos"))]
    let _ = window;
}

