//! Fuzzy picker overlay: files (⌘P in the viewer) and definition candidates.
//! In menu mode (the ⌘⇧S PR menu) it is a short list under section headers,
//! moved through with j/k or picked by row number, with no query field.
//!
//! Matching is fzf-like: the query is split on whitespace and every term must
//! match. A term matches as a substring when it can (strongly preferred,
//! especially at the start of the file name), otherwise as the tightest
//! subsequence. Smart case: a term with an uppercase letter is case-sensitive.
//! A term containing `/` is matched against the whole path, other terms
//! prefer the file name.

use egui::{Key, RichText};

use crate::theme;

pub enum PickerResult {
    Open,
    Cancelled,
    Chosen(usize),
}

/// Most rows drawn (the count shows how many matched in total).
const MAX_ROWS: usize = 300;

pub struct Picker {
    pub title: String,
    /// Placeholder in the empty query field.
    pub hint: String,
    /// (label, dimmed detail) per item.
    pub items: Vec<(String, String)>,
    /// Added to an item's score (and orders an empty query, highest first).
    bonus: Vec<i64>,
    pub query: String,
    /// (item index, matched byte positions in the label), best first.
    pub(crate) filtered: Vec<(usize, Vec<usize>)>,
    matched: usize,
    pub(crate) selected: usize,
    /// Items are still being fetched.
    pub loading: bool,
    /// Shown while `loading`.
    pub loading_note: String,
    /// Menu mode: no query field; j/k and arrows move, 1–9 pick that row,
    /// Enter picks, Esc closes. Items keep their order.
    menu: bool,
    /// Menu mode: (first item index, header) of each section.
    sections: Vec<(usize, String)>,
    /// The user moved the cursor (j/k, arrows) since the picker opened.
    pub(crate) moved: bool,
}

struct Term {
    text: Vec<u8>,
    case_sensitive: bool,
    whole_path: bool,
}

/// A parsed query: whitespace-separated terms that must all match.
pub struct Query {
    terms: Vec<Term>,
}

impl Query {
    pub fn new(q: &str) -> Self {
        let terms = q
            .split_whitespace()
            .map(|t| Term {
                case_sensitive: t.chars().any(|c| c.is_uppercase()),
                whole_path: t.contains('/'),
                text: if t.chars().any(|c| c.is_uppercase()) { t.as_bytes().to_vec() } else { t.to_ascii_lowercase().into_bytes() },
            })
            .collect();
        Self { terms }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Score of `s` (higher is better) and the matched byte positions;
    /// `None` when some term does not match.
    pub fn score(&self, s: &str) -> Option<(i64, Vec<usize>)> {
        let bytes = s.as_bytes();
        let lower = s.to_ascii_lowercase();
        let name_start = s.rfind('/').map(|i| i + 1).unwrap_or(0);
        let mut total = 0i64;
        let mut positions = Vec::new();
        for t in &self.terms {
            let hay = if t.case_sensitive { bytes } else { lower.as_bytes() };
            let in_name = if t.whole_path { None } else { match_term(&t.text, hay, bytes, name_start).map(|(sc, p)| (sc + 60, p)) };
            let whole = match_term(&t.text, hay, bytes, 0);
            let (sc, pos) = match (in_name, whole) {
                (Some(a), Some(b)) => {
                    if a.0 >= b.0 {
                        a
                    } else {
                        b
                    }
                }
                (a, b) => a.or(b)?,
            };
            total += sc;
            positions.extend(pos);
        }
        positions.sort_unstable();
        positions.dedup();
        // Shorter paths win ties.
        Some((total - s.len() as i64 / 4, positions))
    }
}

/// Is byte `i` of `orig` at a word start (after a separator, or camelCase)?
fn boundary(orig: &[u8], i: usize) -> bool {
    i == 0 || matches!(orig[i - 1], b'/' | b'_' | b'-' | b'.' | b' ') || (orig[i - 1].is_ascii_lowercase() && orig[i].is_ascii_uppercase())
}

/// Match one term in `hay[from..]` (`orig` is the original-case string).
fn match_term(t: &[u8], hay: &[u8], orig: &[u8], from: usize) -> Option<(i64, Vec<usize>)> {
    if t.is_empty() || hay.len() < from + t.len() {
        return None;
    }
    let region = &hay[from..];
    // 1. Substring: pick the occurrence with the best start.
    let mut best: Option<(i64, usize)> = None;
    let mut i = 0;
    while let Some(off) = find(&region[i..], t) {
        let at = from + i + off;
        let mut sc = 200 + 16 * t.len() as i64;
        if at == from {
            sc += 60; // starts the file name (or the path)
        } else if boundary(orig, at) {
            sc += 35;
        }
        let end = at + t.len();
        if at == from && (end == hay.len() || orig[end] == b'.') {
            sc += 80; // the whole name / stem
        }
        if best.is_none_or(|(b, _)| sc > b) {
            best = Some((sc, at));
        }
        i += off + 1;
    }
    if let Some((sc, at)) = best {
        return Some((sc, (at..at + t.len()).collect()));
    }
    // 2. Subsequence: find the earliest end greedily, then walk back for the
    // tightest start (fzf v1).
    let mut end = None;
    let mut ti = 0;
    for (j, &c) in region.iter().enumerate() {
        if c == t[ti] {
            ti += 1;
            if ti == t.len() {
                end = Some(from + j);
                break;
            }
        }
    }
    let end = end?;
    let mut pos = Vec::with_capacity(t.len());
    let mut ti = t.len();
    let mut j = end + 1;
    while ti > 0 {
        j -= 1;
        if hay[j] == t[ti - 1] {
            ti -= 1;
            pos.push(j);
        }
    }
    pos.reverse();
    let mut sc = 0i64;
    for (k, &p) in pos.iter().enumerate() {
        sc += 10;
        if k > 0 && pos[k - 1] + 1 == p {
            sc += 14;
        }
        if boundary(orig, p) {
            sc += 22;
        }
    }
    let span = (pos[pos.len() - 1] - pos[0] + 1) as i64;
    sc -= (span - t.len() as i64).min(40);
    Some((sc, pos))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<(String, String)>) -> Self {
        let bonus = vec![0; items.len()];
        Self::with_bonus(title, items, bonus)
    }

    pub fn with_bonus(title: impl Into<String>, items: Vec<(String, String)>, bonus: Vec<i64>) -> Self {
        let mut p = Self {
            title: title.into(),
            hint: "Type to filter".into(),
            items,
            bonus,
            query: String::new(),
            filtered: Vec::new(),
            matched: 0,
            selected: 0,
            loading: false,
            loading_note: "loading files…".into(),
            menu: false,
            sections: Vec::new(),
            moved: false,
        };
        p.refilter();
        p
    }

    /// A menu-mode picker over `items` in their order, under `sections`.
    pub fn menu(title: impl Into<String>, items: Vec<(String, String)>, sections: Vec<(usize, String)>) -> Self {
        // Equal bonuses keep the index order (`refilter` breaks ties by index).
        let mut p = Self::new(title, items);
        p.menu = true;
        p.sections = sections;
        p
    }

    /// The item under the cursor.
    pub fn selected_item(&self) -> Option<usize> {
        self.filtered.get(self.selected).map(|(i, _)| *i)
    }

    /// Put the cursor on item `item`, if it is listed.
    pub fn select(&mut self, item: usize) {
        if let Some(row) = self.filtered.iter().position(|(i, _)| *i == item) {
            self.selected = row;
        }
    }

    /// Menu mode: new items and sections, the cursor staying on its row.
    pub fn set_menu(&mut self, items: Vec<(String, String)>, sections: Vec<(usize, String)>) {
        let keep = self.selected;
        let n = items.len();
        self.set_items(items, vec![0; n]);
        self.sections = sections;
        self.selected = keep.min(n.saturating_sub(1));
    }

    /// Replace the items (e.g. once the file list arrives), keeping the query.
    pub fn set_items(&mut self, items: Vec<(String, String)>, bonus: Vec<i64>) {
        self.items = items;
        self.bonus = bonus;
        self.loading = false;
        self.refilter();
    }

    fn refilter(&mut self) {
        let q = Query::new(&self.query);
        let bonus = |i: usize| self.bonus.get(i).copied().unwrap_or(0);
        let mut scored: Vec<(i64, usize, Vec<usize>)> = if q.is_empty() {
            self.items.iter().enumerate().map(|(i, _)| (bonus(i), i, Vec::new())).collect()
        } else {
            self.items.iter().enumerate().filter_map(|(i, (label, _))| q.score(label).map(|(s, p)| (s + bonus(i), i, p))).collect()
        };
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matched = scored.len();
        self.filtered = scored.into_iter().take(MAX_ROWS).map(|(_, i, p)| (i, p)).collect();
        self.selected = 0;
    }

    /// `focused`: keyboard input belongs to the picker's panel.
    pub fn ui(&mut self, ui: &mut egui::Ui, focused: bool) -> PickerResult {
        let mut result = PickerResult::Open;
        if focused {
            let menu = self.menu;
            if menu {
                // Nothing else types while the menu is up (an open ⌘M draft
                // would take j/k as text).
                ui.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
            }
            let (up, down, enter, esc, digit) = ui.input_mut(|i| {
                let ctrl = egui::Modifiers::CTRL;
                let none = egui::Modifiers::NONE;
                let keys = (
                    i.consume_key(none, Key::ArrowUp) || i.consume_key(ctrl, Key::K) || i.consume_key(ctrl, Key::P) || (menu && i.consume_key(none, Key::K)),
                    i.consume_key(none, Key::ArrowDown) || i.consume_key(ctrl, Key::J) || i.consume_key(ctrl, Key::N) || (menu && i.consume_key(none, Key::J)),
                    i.consume_key(none, Key::Enter),
                    i.consume_key(none, Key::Escape),
                    if menu { DIGITS.iter().position(|k| i.consume_key(none, *k)) } else { None },
                );
                if menu {
                    // The keys' text too, or the viewer's vim reads "3" as a
                    // count once the menu closes this frame.
                    i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
                }
                keys
            });
            if esc {
                return PickerResult::Cancelled;
            }
            if let Some((i, _)) = digit.and_then(|d| self.filtered.get(d)) {
                return PickerResult::Chosen(*i);
            }
            self.moved |= up || down;
            if up {
                self.selected = self.selected.saturating_sub(1);
            }
            if down && self.selected + 1 < self.filtered.len() {
                self.selected += 1;
            }
            if enter {
                if let Some((i, _)) = self.filtered.get(self.selected) {
                    return PickerResult::Chosen(*i);
                }
            }
        }
        crate::ui::widgets::overlay_frame().inner_margin(egui::Margin::ZERO).show(ui, |ui| {
            // Title and count.
            egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 12, bottom: 2 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(crate::ui::widgets::section_label(&self.title, theme::SUBTLE));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let count = if self.loading { self.loading_note.clone() } else { format!("{} / {}", self.matched, self.items.len()) };
                        ui.label(RichText::new(count).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
                    });
                });
            });
            if self.menu {
                egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 2, bottom: 8 }).show(ui, |ui| {
                    ui.label(RichText::new(&self.hint).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
                });
            } else {
                let r = egui::Frame::NONE
                    .inner_margin(egui::Margin { left: 10, right: 10, top: 2, bottom: 8 })
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.query)
                                .id_salt("picker-query")
                                .font(theme::ui_font(theme::TEXT_TITLE))
                                .frame(egui::Frame::NONE)
                                .margin(egui::Margin::symmetric(6, 4))
                                .desired_width(f32::INFINITY)
                                .hint_text(RichText::new(&self.hint).font(theme::ui_font(theme::TEXT_TITLE)).color(theme::DIM)),
                        )
                    })
                    .inner;
                // The picker is modal: keep the query field focused whenever
                // its panel is (focus can be lost by switching panes).
                if focused && !r.has_focus() {
                    r.request_focus();
                }
                if r.changed() {
                    self.refilter();
                }
            }
            let line = ui.available_rect_before_wrap();
            ui.painter().hline(line.x_range(), line.top(), egui::Stroke::new(1.0, theme::BORDER));
            egui::Frame::NONE.inner_margin(egui::Margin::symmetric(6, 6)).show(ui, |ui| {
                let max = (ui.available_height() - 34.0).max(120.0);
                egui::ScrollArea::vertical().max_height(max).auto_shrink([false, true]).show(ui, |ui| {
                    for (row, (i, pos)) in self.filtered.iter().enumerate() {
                        let (label, detail) = &self.items[*i];
                        let sel = row == self.selected;
                        if let Some((_, header)) = self.sections.iter().find(|(first, _)| first == i) {
                            ui.add_space(if row == 0 { 0.0 } else { 6.0 });
                            ui.horizontal(|ui| {
                                ui.add_space(10.0);
                                ui.label(crate::ui::widgets::section_label(header, theme::DIM));
                            });
                        }
                        let job = if self.menu { numbered(row, label, sel) } else { highlighted(label, pos, sel) };
                        let resp = picker_row(ui, sel, job, detail);
                        if sel {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked_by(egui::PointerButton::Primary) {
                            result = PickerResult::Chosen(*i);
                        }
                    }
                    let note = |ui: &mut egui::Ui, text: &str| {
                        ui.horizontal(|ui| {
                            ui.add_space(10.0);
                            ui.label(RichText::new(text).font(theme::ui_font(theme::TEXT_SMALL)).color(theme::SUBTLE));
                        });
                    };
                    if self.filtered.is_empty() {
                        note(ui, if self.loading { &self.loading_note } else if self.menu { "nothing to show" } else { "no matches" });
                    } else if self.matched > self.filtered.len() {
                        note(ui, &format!("… {} more — keep typing", self.matched - self.filtered.len()));
                    }
                });
            });
            let line = ui.available_rect_before_wrap();
            ui.painter().hline(line.x_range(), line.top(), egui::Stroke::new(1.0, theme::BORDER));
            egui::Frame::NONE.inner_margin(egui::Margin { left: 16, right: 16, top: 7, bottom: 8 }).show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.menu {
                        crate::ui::widgets::key_hints(ui, &[("j/k", "select"), ("1–9", "jump"), ("⏎", "show"), ("esc", "close")]);
                        return;
                    }
                    crate::ui::widgets::key_hints(ui, &[("↑↓", "select"), ("⏎", "open"), ("esc", "close")]);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("spaces separate terms").font(theme::ui_font(theme::TEXT_SMALL)).color(theme::DIM));
                    });
                });
            });
        });
        result
    }
}

/// One result: the label, and its detail right-aligned and dimmer.
fn picker_row(ui: &mut egui::Ui, selected: bool, label: egui::text::LayoutJob, detail: &str) -> egui::Response {
    let width = ui.available_width();
    let pad = egui::vec2(10.0, 5.0);
    let detail = (!detail.is_empty()).then(|| ui.painter().layout_no_wrap(detail.to_string(), theme::ui_font(theme::TEXT_SMALL), if selected { theme::SUBTLE } else { theme::DIM }));
    let detail_w = detail.as_ref().map_or(0.0, |g| (g.size().x + 16.0).min(width * 0.45));
    let mut label = label;
    label.wrap = egui::text::TextWrapping::truncate_at_width(width - 2.0 * pad.x - detail_w);
    let galley = ui.painter().layout_job(label);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, galley.size().y + 2.0 * pad.y), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let fill = if selected {
            theme::SELECTED
        } else if resp.hovered() {
            theme::HOVER
        } else {
            egui::Color32::TRANSPARENT
        };
        ui.painter().rect_filled(rect, egui::CornerRadius::same(theme::RADIUS), fill);
        ui.painter().galley(rect.min + pad, galley, theme::TEXT);
        if let Some(g) = detail {
            let clip = egui::Rect::from_min_max(egui::pos2(rect.right() - detail_w, rect.top()), rect.max);
            ui.painter().with_clip_rect(clip).galley(egui::pos2(rect.right() - pad.x - g.size().x, rect.center().y - g.size().y / 2.0), g, theme::DIM);
        }
    }
    resp
}

/// Keys that pick menu rows 1–9.
const DIGITS: [Key; 9] = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];

/// A menu row: its number (1–9, the key that picks it) and its label, plain.
fn numbered(row: usize, label: &str, selected: bool) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::monospace(12.5);
    let number = if row < DIGITS.len() { format!("{}  ", row + 1) } else { "   ".into() };
    job.append(&number, 0.0, egui::TextFormat { font_id: font.clone(), color: theme::DIM, ..Default::default() });
    job.append(label, 0.0, egui::TextFormat { font_id: font, color: if selected { theme::TEXT } else { theme::MUTED }, ..Default::default() });
    job
}

/// `label` with the matched bytes in the accent color; the file name brighter than its directory.
fn highlighted(label: &str, pos: &[usize], selected: bool) -> egui::text::LayoutJob {
    let name_start = label.rfind('/').map(|i| i + 1).unwrap_or(0);
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::monospace(12.5);
    let mut pi = 0;
    for (b, c) in label.char_indices() {
        while pi < pos.len() && pos[pi] < b {
            pi += 1;
        }
        let hit = pi < pos.len() && pos[pi] == b;
        let base = match (selected, b >= name_start) {
            (true, true) => theme::TEXT,
            (true, false) => theme::MUTED,
            (false, true) => theme::MUTED,
            (false, false) => theme::SUBTLE,
        };
        let color = if hit { theme::ACCENT } else { base };
        let mut buf = [0u8; 4];
        job.append(c.encode_utf8(&mut buf), 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(q: &str, items: &[&str]) -> Vec<String> {
        let q = Query::new(q);
        let mut v: Vec<(i64, &str)> = items.iter().filter_map(|s| q.score(s).map(|(sc, _)| (sc, *s))).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0));
        v.into_iter().map(|(_, s)| s.to_string()).collect()
    }

    const PATHS: &[&str] = &[
        "aster/src/main.rs",
        "aster/server/handlers/chat.py",
        "rust/lib.rs",
        "README.md",
        "aster/docs/README.md",
        "crates/app/src/viewer/vim.rs",
        "crates/v/i/m/other.rs",
        "search/aster_client/api.py",
        "tools/remain_now.txt",
    ];

    #[test]
    fn substring_and_name_first() {
        assert_eq!(rank("main", PATHS)[0], "aster/src/main.rs");
        assert_eq!(rank("vim", PATHS)[0], "crates/app/src/viewer/vim.rs");
        assert_eq!(rank("readme", PATHS)[0], "README.md", "shorter path wins among exact names");
        assert!(rank("xyz", PATHS).is_empty());
    }

    #[test]
    fn multiple_terms_and_paths() {
        assert_eq!(rank("aster chat", PATHS)[0], "aster/server/handlers/chat.py");
        assert_eq!(rank("docs/ readme", PATHS)[0], "aster/docs/README.md");
        assert_eq!(rank("srv hand", PATHS)[0], "aster/server/handlers/chat.py");
        assert_eq!(rank("ac/api", PATHS)[0], "search/aster_client/api.py");
    }

    #[test]
    fn smart_case_and_positions() {
        assert_eq!(rank("README", PATHS).len(), 2);
        assert!(rank("Main", PATHS).is_empty(), "uppercase is case-sensitive");
        let (_, pos) = Query::new("main").score("aster/src/main.rs").unwrap();
        assert_eq!(pos, vec![10, 11, 12, 13]);
    }

    /// The PR menu: j/k move, a digit picks its row (and its text never
    /// reaches the viewer), Esc closes, a new menu keeps the cursor's row,
    /// and a focused text field gives the keyboard up.
    #[test]
    fn menu_keys_move_pick_and_cancel() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let items = |n: usize| (0..n).map(|i| (format!("#{i}"), String::new())).collect::<Vec<_>>();
        let mut p = Picker::menu("PRs", items(4), vec![(0, "main ← a".into()), (2, "main ← c".into())]);
        let key = |key| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let field = egui::Id::new("draft");
        let frame = |p: &mut Picker, events: Vec<egui::Event>| {
            let mut result = PickerResult::Open;
            let mut left = Vec::new();
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                result = p.ui(ui, true);
                left = ui.input(|i| i.events.clone());
            });
            out.textures_delta.clear();
            (result, left)
        };
        ctx.memory_mut(|m| m.request_focus(field));
        for k in [egui::Key::J, egui::Key::J, egui::Key::K] {
            assert!(matches!(frame(&mut p, vec![key(k), egui::Event::Text(format!("{k:?}").to_lowercase())]).0, PickerResult::Open));
        }
        assert_eq!(p.selected, 1);
        assert!(!ctx.memory(|m| m.has_focus(field)), "an open ⌘M draft doesn't get the keys");
        let (r, left) = frame(&mut p, vec![key(egui::Key::Num3), egui::Event::Text("3".into())]);
        assert!(matches!(r, PickerResult::Chosen(2)));
        assert!(!left.iter().any(|e| matches!(e, egui::Event::Text(_))), "vim never reads the 3 as a count");
        assert!(matches!(frame(&mut p, vec![key(egui::Key::Num9)]).0, PickerResult::Open), "no row 9");
        p.selected = 3;
        p.set_menu(items(3), vec![(0, "main ← a".into())]);
        assert_eq!(p.selected, 2, "clamped to the new rows");
        assert_eq!(p.items[p.filtered[0].0].0, "#0", "items keep their order");
        assert!(matches!(frame(&mut p, vec![key(egui::Key::Escape)]).0, PickerResult::Cancelled));
    }

    #[test]
    fn typing_filters_and_late_items() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx); // the UI font families
        let mut p = Picker::new("t", Vec::new());
        p.loading = true;
        let frame = |p: &mut Picker, events: Vec<egui::Event>| {
            let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                p.ui(ui, true);
            });
            out.textures_delta.clear();
        };
        for events in [vec![], vec![egui::Event::Text("ma".into())], vec![egui::Event::Text("in".into())]] {
            frame(&mut p, events);
        }
        assert_eq!(p.query, "main");
        assert!(p.filtered.is_empty());
        // The file list arrives after the user started typing.
        let items: Vec<(String, String)> = PATHS.iter().map(|s| (s.to_string(), String::new())).collect();
        p.set_items(items, vec![0; PATHS.len()]);
        frame(&mut p, vec![]);
        assert_eq!(p.items[p.filtered[0].0].0, "aster/src/main.rs");
    }
}
