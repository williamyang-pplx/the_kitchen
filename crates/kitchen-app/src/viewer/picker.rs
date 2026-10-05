//! Fuzzy picker overlay: files (⌘P in the viewer) and definition candidates.
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
    /// (label, dimmed detail) per item.
    pub items: Vec<(String, String)>,
    /// Added to an item's score (and orders an empty query, highest first).
    bonus: Vec<i64>,
    pub query: String,
    /// (item index, matched byte positions in the label), best first.
    pub(crate) filtered: Vec<(usize, Vec<usize>)>,
    matched: usize,
    selected: usize,
    /// Items are still being fetched.
    pub loading: bool,
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
        let mut p = Self { title: title.into(), items, bonus, query: String::new(), filtered: Vec::new(), matched: 0, selected: 0, loading: false };
        p.refilter();
        p
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
            let (up, down, enter, esc) = ui.input_mut(|i| {
                let ctrl = egui::Modifiers::CTRL;
                (
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) || i.consume_key(ctrl, Key::K) || i.consume_key(ctrl, Key::P),
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) || i.consume_key(ctrl, Key::J) || i.consume_key(ctrl, Key::N),
                    i.consume_key(egui::Modifiers::NONE, Key::Enter),
                    i.consume_key(egui::Modifiers::NONE, Key::Escape),
                )
            });
            if esc {
                return PickerResult::Cancelled;
            }
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
        egui::Frame::NONE.fill(theme::PANEL).stroke(egui::Stroke::new(1.0, theme::ACCENT)).inner_margin(8.0).corner_radius(6.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&self.title).size(11.0).color(theme::ACCENT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let count = if self.loading { "loading files…".to_string() } else { format!("{} / {}", self.matched, self.items.len()) };
                    ui.label(RichText::new(count).size(11.0).color(theme::DIM));
                });
            });
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id_salt("picker-query")
                    .desired_width(f32::INFINITY)
                    .hint_text("type to filter · space-separated terms · ↑↓/ctrl-j/k · Enter · Esc"),
            );
            // The picker is modal: keep the query field focused whenever
            // its panel is (focus can be lost by switching panes).
            if focused && !r.has_focus() {
                r.request_focus();
            }
            if r.changed() {
                self.refilter();
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(ui.available_height().max(120.0)).auto_shrink([false, true]).show(ui, |ui| {
                for (row, (i, pos)) in self.filtered.iter().enumerate() {
                    let (label, detail) = &self.items[*i];
                    let sel = row == self.selected;
                    let mut job = highlighted(label, pos, sel);
                    if !detail.is_empty() {
                        let fmt = egui::TextFormat { font_id: egui::FontId::proportional(11.0), color: theme::DIM, ..Default::default() };
                        job.append(detail, 12.0, fmt);
                    }
                    let resp = super::list_row(ui, sel, job);
                    if sel {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked_by(egui::PointerButton::Primary) {
                        result = PickerResult::Chosen(*i);
                    }
                }
                if self.filtered.is_empty() {
                    let msg = if self.loading { "loading files…" } else { "no matches" };
                    ui.label(RichText::new(msg).color(theme::DIM));
                } else if self.matched > self.filtered.len() {
                    ui.label(RichText::new(format!("… {} more — keep typing", self.matched - self.filtered.len())).size(11.0).color(theme::DIM));
                }
            });
        });
        result
    }
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
            (true, _) => theme::TEXT,
            (false, true) => theme::TEXT.gamma_multiply(0.85),
            (false, false) => theme::DIM,
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

    #[test]
    fn typing_filters_and_late_items() {
        let ctx = egui::Context::default();
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
