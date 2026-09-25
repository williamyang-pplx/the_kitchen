//! Fuzzy picker overlay: files (⌘P in the viewer) and definition candidates.

use egui::{Key, RichText};

use crate::theme;

pub enum PickerResult {
    Open,
    Cancelled,
    Chosen(usize),
}

pub struct Picker {
    pub title: String,
    /// (label, dimmed detail) per item.
    pub items: Vec<(String, String)>,
    pub query: String,
    filtered: Vec<usize>,
    selected: usize,
    focus: bool,
}

/// Subsequence match score (higher is better); `None` when `q` does not
/// match. A match inside the file name beats one spread over directories.
pub fn score(q: &str, s: &str) -> Option<i64> {
    let whole = subsequence_score(q, s)?;
    let name = &s[s.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    let in_name = subsequence_score(q, name).map(|n| n + 40 - s.len() as i64 / 8);
    Some(whole.max(in_name.unwrap_or(i64::MIN)))
}

fn subsequence_score(q: &str, s: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(0);
    }
    let s_lower: Vec<char> = s.to_lowercase().chars().collect();
    let orig: Vec<char> = s.chars().collect();
    let mut score = 0i64;
    let mut si = 0;
    let mut prev_match: Option<usize> = None;
    for qc in q.to_lowercase().chars() {
        let found = (si..s_lower.len()).find(|&i| s_lower[i] == qc)?;
        score += 10;
        if prev_match == Some(found.wrapping_sub(1)) {
            score += 15; // consecutive
        }
        let boundary = found == 0 || matches!(orig.get(found - 1), Some('/' | '_' | '-' | '.' | ' '));
        if boundary {
            score += 20;
        }
        score -= (found - si) as i64;
        prev_match = Some(found);
        si = found + 1;
    }
    // Prefer matches in the file name and shorter paths.
    let name_start = s.rfind('/').map(|i| i + 1).unwrap_or(0);
    if prev_match.map(|p| p >= name_start).unwrap_or(false) {
        score += 25;
    }
    Some(score - s.len() as i64 / 8)
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<(String, String)>) -> Self {
        let mut p = Self { title: title.into(), items, query: String::new(), filtered: Vec::new(), selected: 0, focus: true };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        let mut scored: Vec<(i64, usize)> =
            self.items.iter().enumerate().filter_map(|(i, (label, _))| score(&self.query, label).map(|s| (s, i))).collect();
        if !self.query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        self.filtered = scored.into_iter().map(|(_, i)| i).take(200).collect();
        self.selected = 0;
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> PickerResult {
        let mut result = PickerResult::Open;
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
            if let Some(&i) = self.filtered.get(self.selected) {
                return PickerResult::Chosen(i);
            }
        }
        egui::Frame::NONE.fill(theme::PANEL).stroke(egui::Stroke::new(1.0, theme::ACCENT)).inner_margin(8.0).corner_radius(6.0).show(ui, |ui| {
            ui.label(RichText::new(&self.title).size(11.0).color(theme::ACCENT));
            let r = ui.add(egui::TextEdit::singleline(&mut self.query).desired_width(f32::INFINITY).hint_text("type to filter · ↑↓/ctrl-j/k · Enter · Esc"));
            if self.focus {
                r.request_focus();
                self.focus = false;
            }
            if r.changed() {
                self.refilter();
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| {
                for (row, &i) in self.filtered.iter().enumerate() {
                    let (label, detail) = &self.items[i];
                    let sel = row == self.selected;
                    let text = RichText::new(label).monospace().size(12.5).color(if sel { theme::TEXT } else { theme::DIM });
                    let resp = ui.horizontal(|ui| {
                        let r = super::list_row(ui, sel, text);
                        if !detail.is_empty() {
                            ui.label(RichText::new(detail).size(11.0).color(theme::DIM));
                        }
                        r
                    });
                    if sel {
                        resp.inner.scroll_to_me(None);
                    }
                    if resp.inner.clicked() {
                        result = PickerResult::Chosen(i);
                    }
                }
                if self.filtered.is_empty() {
                    ui.label(RichText::new("no matches").color(theme::DIM));
                }
            });
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoring() {
        assert!(score("app", "src/app.py").is_some());
        assert!(score("xyz", "src/app.py").is_none());
        let a = score("vim", "crates/app/src/viewer/vim.rs").unwrap();
        let b = score("vim", "crates/v/i/m/other.rs").unwrap();
        assert!(a > b, "{a} vs {b}");
    }
}
