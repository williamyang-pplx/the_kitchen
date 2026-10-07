//! What the code view displays: lines with diff kind, line numbers and
//! highlight spans. Built from a plain file, a base/head diff, an outline,
//! or a plugin's rendering of either.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use egui::Color32;

use crate::plugins::decor::RenderRow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Normal,
    Added,
    Removed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DocLine {
    /// Tabs expanded, no trailing newline.
    pub text: String,
    pub kind: LineKind,
    /// 1-based line in the base version (diff mode).
    pub old_no: Option<usize>,
    /// 1-based line in the current version.
    pub new_no: Option<usize>,
    /// Dim text drawn after the line (outline summaries).
    pub suffix: Option<String>,
    /// Byte ranges of `text` (tab-expanded) with a color.
    pub spans: Vec<(Range<usize>, Color32)>,
    /// A rendered row (`kitchen.render`) standing for current-file lines
    /// `new_no..=last_no()`; painted from this instead of `spans`.
    pub render: Option<Arc<RenderRow>>,
}

impl DocLine {
    pub fn plain(text: String, new_no: usize) -> Self {
        Self { text, kind: LineKind::Normal, old_no: Some(new_no), new_no: Some(new_no), suffix: None, spans: Vec::new(), render: None }
    }

    /// The last current-file line this row shows (a rendered row may cover several).
    pub fn last_no(&self) -> Option<usize> {
        let n = self.new_no?;
        Some(self.render.as_ref().map_or(n, |r| r.last_line().max(n)))
    }

    /// Shows current-file line `n`.
    pub fn covers_new(&self, n: usize) -> bool {
        self.new_no.is_some_and(|first| first <= n && self.last_no().is_some_and(|last| n <= last))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub lines: Vec<DocLine>,
}

pub fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

impl Doc {
    pub fn texts(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.text.clone()).collect()
    }

    pub fn plain(text: &str, spans: Option<&[Vec<(Range<usize>, Color32)>]>) -> Doc {
        let lines = text
            .lines()
            .enumerate()
            .map(|(i, l)| {
                let mut d = DocLine::plain(expand_tabs(l), i + 1);
                if let Some(s) = spans.and_then(|s| s.get(i)) {
                    d.spans = s.clone();
                }
                d
            })
            .collect();
        Doc { lines }
    }

    /// Whole-file unified diff: every line of `new`, with removed base lines
    /// interleaved where they were deleted.
    pub fn diff(
        old: &str,
        new: &str,
        old_spans: Option<&[Vec<(Range<usize>, Color32)>]>,
        new_spans: Option<&[Vec<(Range<usize>, Color32)>]>,
    ) -> Doc {
        use similar::{ChangeTag, TextDiff};
        let diff = TextDiff::from_lines(old, new);
        let mut lines = Vec::new();
        for change in diff.iter_all_changes() {
            let raw = change.value().trim_end_matches(['\n', '\r']);
            let (kind, old_no, new_no) = match change.tag() {
                ChangeTag::Equal => (LineKind::Normal, change.old_index().map(|i| i + 1), change.new_index().map(|i| i + 1)),
                ChangeTag::Delete => (LineKind::Removed, change.old_index().map(|i| i + 1), None),
                ChangeTag::Insert => (LineKind::Added, None, change.new_index().map(|i| i + 1)),
            };
            let spans = match kind {
                LineKind::Removed => old_no.and_then(|n| old_spans.and_then(|s| s.get(n - 1))),
                _ => new_no.and_then(|n| new_spans.and_then(|s| s.get(n - 1))),
            };
            let spans = spans.cloned().unwrap_or_default();
            lines.push(DocLine { text: expand_tabs(raw), kind, old_no, new_no, suffix: None, spans, render: None });
        }
        Doc { lines }
    }

    /// `src` (a plain or diff doc) with a plugin's rendering: each row in
    /// place of the lines it covers, marked added when one of them is.
    /// Removed lines and lines no row covers stay as they are.
    pub fn rendered(src: &Doc, rows: &[RenderRow]) -> Doc {
        let max = src.lines.iter().filter_map(|l| l.new_no).max().unwrap_or(0);
        let mut owner: HashMap<usize, usize> = HashMap::new();
        for (i, r) in rows.iter().enumerate() {
            for n in r.line..=r.last_line().min(max) {
                owner.entry(n).or_insert(i);
            }
        }
        let mut added = vec![false; rows.len()];
        for l in src.lines.iter().filter(|l| l.kind == LineKind::Added) {
            if let Some(&i) = l.new_no.and_then(|n| owner.get(&n)) {
                added[i] = true;
            }
        }
        let mut shown = vec![false; rows.len()];
        let mut lines = Vec::new();
        for l in &src.lines {
            let row = l.new_no.filter(|_| l.kind != LineKind::Removed).and_then(|n| owner.get(&n).copied());
            let Some(i) = row else {
                lines.push(l.clone());
                continue;
            };
            if std::mem::replace(&mut shown[i], true) {
                continue;
            }
            let r = &rows[i];
            // One display row: no line breaks; tabs as one space keep the span columns.
            let text: String = r.text.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).collect();
            let mut row = r.clone();
            row.text = text.clone();
            let kind = if added[i] { LineKind::Added } else { LineKind::Normal };
            lines.push(DocLine { text, kind, old_no: None, new_no: Some(r.line), suffix: None, spans: Vec::new(), render: Some(Arc::new(row)) });
        }
        Doc { lines }
    }

    /// Start indices of each run of changed lines.
    pub fn hunk_starts(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut prev_changed = false;
        for (i, l) in self.lines.iter().enumerate() {
            let changed = l.kind != LineKind::Normal;
            if changed && !prev_changed {
                out.push(i);
            }
            prev_changed = changed;
        }
        out
    }

    /// Doc index showing current-file line `n` (or the nearest after it).
    pub fn index_of_new(&self, n: usize) -> Option<usize> {
        self.lines.iter().position(|l| l.covers_new(n) || l.new_no.is_some_and(|x| x >= n))
    }

    pub fn changed_count(&self) -> (usize, usize) {
        let add = self.lines.iter().filter(|l| l.kind == LineKind::Added).count();
        let del = self.lines.iter().filter(|l| l.kind == LineKind::Removed).count();
        (add, del)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_interleaves_removed_lines() {
        let d = Doc::diff("a\nb\nc\nd\n", "a\nB\nc\nd\ne\n", None, None);
        let kinds: Vec<(LineKind, Option<usize>, Option<usize>, &str)> =
            d.lines.iter().map(|l| (l.kind, l.old_no, l.new_no, l.text.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (LineKind::Normal, Some(1), Some(1), "a"),
                (LineKind::Removed, Some(2), None, "b"),
                (LineKind::Added, None, Some(2), "B"),
                (LineKind::Normal, Some(3), Some(3), "c"),
                (LineKind::Normal, Some(4), Some(4), "d"),
                (LineKind::Added, None, Some(5), "e"),
            ]
        );
        assert_eq!(d.hunk_starts(), vec![1, 5]);
        assert_eq!(d.index_of_new(2), Some(2));
        assert_eq!(d.changed_count(), (2, 1));
    }

    fn row(line: usize, end_line: Option<usize>, text: &str) -> RenderRow {
        RenderRow { line, end_line, text: text.into(), ..Default::default() }
    }

    #[test]
    fn rendered_rows_replace_the_lines_they_cover() {
        let rows = [row(1, None, "Title"), row(3, Some(4), "one two"), row(9, None, "past the end")];
        let d = Doc::rendered(&Doc::plain("# Title\n\none\ntwo\nraw\n", None), &rows);
        let got: Vec<(Option<usize>, Option<usize>, &str, bool)> = d.lines.iter().map(|l| (l.new_no, l.last_no(), l.text.as_str(), l.render.is_some())).collect();
        assert_eq!(
            got,
            vec![(Some(1), Some(1), "Title", true), (Some(2), Some(2), "", false), (Some(3), Some(4), "one two", true), (Some(5), Some(5), "raw", false)],
            "uncovered lines stay raw; rows past the end go"
        );
        assert_eq!(d.index_of_new(4), Some(2), "a line inside a row finds the row");
        assert!(d.lines[2].covers_new(3) && d.lines[2].covers_new(4) && !d.lines[2].covers_new(5));

        // On a diff: removed lines stay, a row with an added line is added.
        let diff = Doc::diff("a\nb\nc\n", "a\nB\nc\n", None, None);
        let d = Doc::rendered(&diff, &[row(1, Some(2), "a B"), row(3, None, "c")]);
        let got: Vec<(LineKind, Option<usize>, &str)> = d.lines.iter().map(|l| (l.kind, l.new_no, l.text.as_str())).collect();
        assert_eq!(got, vec![(LineKind::Added, Some(1), "a B"), (LineKind::Removed, None, "b"), (LineKind::Normal, Some(3), "c")]);
    }

    #[test]
    fn plain_and_tabs() {
        let d = Doc::plain("x\n\ty\n", None);
        assert_eq!(d.lines[1].text, "    y");
        assert_eq!(d.lines[1].new_no, Some(2));
        assert_eq!(expand_tabs("ab\tc"), "ab  c");
    }
}
