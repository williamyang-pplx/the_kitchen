//! What the code view displays: lines with diff kind, line numbers and
//! highlight spans. Built from a plain file, a base/head diff, an outline,
//! or a plugin's rendering of either. A diff can be folded: runs of
//! unchanged rows far from a change shrink to one fold row.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use egui::Color32;

use crate::plugins::decor::RenderRow;

/// Unchanged rows kept on each side of a change in a folded diff
/// (`git diff -U3`; GitHub only takes comments on these lines).
pub const CONTEXT: usize = 3;
/// A shorter run of foldable rows stays: a fold row would save almost no room.
const MIN_FOLD: usize = 4;

/// Unchanged lines a folded diff hides behind one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    /// First and last hidden current-file line (1-based).
    pub first: usize,
    pub last: usize,
    /// Base-side line of `first`; `None` when every hidden row is a plugin
    /// rendering (rendered rows have no base numbers). Unchanged lines move
    /// together, so the rest follow it.
    pub old_first: Option<usize>,
}

impl Fold {
    /// Hides base line `n`.
    pub fn covers_old(&self, n: usize) -> bool {
        self.old_first.is_some_and(|o| o <= n && n - o <= self.last - self.first)
    }
}

/// The folds opened in one file's diff. The viewer keeps one per path for
/// its lifetime and keys the doc on it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unfolded {
    /// zR: nothing is folded.
    pub all: bool,
    /// `Fold::first` of each fold opened one at a time (zo, Enter, a jump
    /// into it). A run is either all open or all folded.
    pub opened: HashSet<usize>,
}

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
    /// A fold row: stands for these hidden lines. Its `text` is empty (so
    /// search, `gd` and the plugin `word` see nothing), `kind` is `Normal`,
    /// and `old_no` / `new_no` are `None` (no gutter numbers).
    pub fold: Option<Fold>,
}

impl DocLine {
    pub fn plain(text: String, new_no: usize) -> Self {
        Self { text, kind: LineKind::Normal, old_no: Some(new_no), new_no: Some(new_no), suffix: None, spans: Vec::new(), render: None, fold: None }
    }

    /// The first current-file line this row shows (a fold: the first one it hides).
    pub fn first_no(&self) -> Option<usize> {
        self.new_no.or(self.fold.map(|f| f.first))
    }

    /// The last current-file line this row shows (a rendered row may cover
    /// several; a fold row, every line it hides).
    pub fn last_no(&self) -> Option<usize> {
        if let Some(f) = self.fold {
            return Some(f.last);
        }
        let n = self.new_no?;
        Some(self.render.as_ref().map_or(n, |r| r.last_line().max(n)))
    }

    /// Shows current-file line `n` (a fold row: hides it).
    pub fn covers_new(&self, n: usize) -> bool {
        self.first_no().is_some_and(|first| first <= n && self.last_no().is_some_and(|last| n <= last))
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
            lines.push(DocLine { text: expand_tabs(raw), kind, old_no, new_no, suffix: None, spans, render: None, fold: None });
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
            // The row's text is its source on one line, for search.
            let text: String = r.markdown.chars().map(|c| if c == '\n' || c == '\t' { ' ' } else { c }).collect();
            let kind = if added[i] { LineKind::Added } else { LineKind::Normal };
            lines.push(DocLine { text, kind, old_no: None, new_no: Some(r.line), suffix: None, spans: Vec::new(), render: Some(Arc::new(r.clone())), fold: None });
        }
        Doc { lines }
    }

    /// `self` (a diff, maybe rendered) with each run of at least MIN_FOLD
    /// rows more than CONTEXT rows from a change behind one fold row, unless
    /// `open` opened it. Distances and run lengths count rows, not lines: a
    /// rendered row that stands for several lines is one row on screen.
    /// A doc with no change, or with `open.all`, comes back whole.
    pub fn folded(&self, open: &Unfolded) -> Doc {
        let changed: Vec<bool> = self.lines.iter().map(|l| l.kind != LineKind::Normal).collect();
        if open.all || !changed.contains(&true) {
            return self.clone();
        }
        // Rows from the nearest change, looking back and then ahead.
        let n = self.lines.len();
        let mut dist = vec![usize::MAX; n];
        let mut last = None;
        for i in 0..n {
            if changed[i] {
                last = Some(i);
            }
            dist[i] = last.map_or(usize::MAX, |c| i - c);
        }
        last = None;
        for i in (0..n).rev() {
            if changed[i] {
                last = Some(i);
            }
            dist[i] = dist[i].min(last.map_or(usize::MAX, |c| c - i));
        }
        let mut lines = Vec::with_capacity(n);
        let mut i = 0;
        while i < n {
            if dist[i] <= CONTEXT {
                lines.push(self.lines[i].clone());
                i += 1;
                continue;
            }
            let mut e = i + 1;
            while e < n && dist[e] > CONTEXT {
                e += 1;
            }
            let run = &self.lines[i..e];
            let span = run[0].first_no().zip(run[run.len() - 1].last_no());
            match span {
                Some((first, last)) if run.len() >= MIN_FOLD && !open.opened.contains(&first) => {
                    // A rendered first row has no base number: count back
                    // from the first row that has one, in lines.
                    let old_first = run.iter().find_map(|l| Some(l.old_no? - (l.first_no()? - first)));
                    let fold = Fold { first, last, old_first };
                    lines.push(DocLine { text: String::new(), kind: LineKind::Normal, old_no: None, new_no: None, suffix: None, spans: Vec::new(), render: None, fold: Some(fold) });
                }
                _ => lines.extend_from_slice(run),
            }
            i = e;
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

    fn numbered(n: usize, changed: &[usize]) -> String {
        (1..=n).map(|i| if changed.contains(&i) { format!("x{i}\n") } else { format!("{i}\n") }).collect()
    }

    /// Fold rows as (first, last), other rows as their current-file line (0: removed).
    fn shape(d: &Doc) -> Vec<(usize, usize)> {
        d.lines.iter().map(|l| l.fold.map_or((l.new_no.unwrap_or(0), l.new_no.unwrap_or(0)), |f| (f.first, f.last))).collect()
    }

    #[test]
    fn folded_diff_hides_far_unchanged_runs() {
        let d = Doc::diff(&numbered(30, &[]), &numbered(30, &[15]), None, None);
        let f = d.folded(&Unfolded::default());
        let mid = [(12, 12), (13, 13), (14, 14), (0, 0), (15, 15), (16, 16), (17, 17), (18, 18)];
        let want: Vec<(usize, usize)> = [(1, 11)].into_iter().chain(mid).chain([(19, 30)]).collect();
        assert_eq!(shape(&f), want);
        let first = &f.lines[0];
        assert_eq!((first.text.as_str(), first.kind, first.old_no, first.new_no), ("", LineKind::Normal, None, None));
        assert_eq!(first.fold.unwrap().old_first, Some(1));
        assert_eq!(f.hunk_starts(), vec![4]);
        assert_eq!(f.changed_count(), (1, 1));
        assert_eq!(f.index_of_new(5), Some(0), "a folded line finds its fold");
        assert_eq!(first.first_no(), Some(1));
        let end = &f.lines[f.lines.len() - 1];
        assert!(end.covers_new(30) && !end.covers_new(18));
        assert!(end.fold.unwrap().covers_old(30) && !end.fold.unwrap().covers_old(31));
        // Opening the first fold shows its lines; the other stays folded.
        let open = Unfolded { all: false, opened: HashSet::from([1]) };
        let f = d.folded(&open);
        assert_eq!(f.lines.len(), 11 + mid.len() + 1);
        assert!(f.lines[..11].iter().all(|l| l.fold.is_none()) && f.lines[f.lines.len() - 1].fold.is_some());
    }

    #[test]
    fn folded_leaves_short_runs_and_unchanged_docs() {
        let folds = |d: &Doc| d.lines.iter().filter(|l| l.fold.is_some()).count();
        // Changes 10 lines apart (9 between): 3 rows left after the context stay.
        let d = Doc::diff(&numbered(11, &[]), &numbered(11, &[1, 11]), None, None);
        assert_eq!(folds(&d.folded(&Unfolded::default())), 0);
        // 10 between: 4 rows left, one fold.
        let d = Doc::diff(&numbered(12, &[]), &numbered(12, &[1, 12]), None, None);
        let f = d.folded(&Unfolded::default());
        assert_eq!(f.lines.iter().filter_map(|l| l.fold).map(|f| (f.first, f.last)).collect::<Vec<_>>(), vec![(5, 8)]);
        assert_eq!(folds(&d.folded(&Unfolded { all: true, ..Default::default() })), 0, "zR: nothing folded");
        let same = Doc::diff(&numbered(30, &[]), &numbered(30, &[]), None, None);
        assert_eq!(folds(&same.folded(&Unfolded::default())), 0, "no change: the whole file");
        // A rendered first row has no base number: the fold counts back from the next row.
        let r = Doc::rendered(&Doc::diff(&numbered(20, &[]), &numbered(20, &[20]), None, None), &[row(1, Some(2), "one two")]).folded(&Unfolded::default());
        let fold = r.lines[0].fold.expect("rows 1..=16 fold");
        assert_eq!((fold.first, fold.last, fold.old_first), (1, 16, Some(1)));
    }

    fn row(line: usize, end_line: Option<usize>, text: &str) -> RenderRow {
        RenderRow { line, end_line, markdown: text.into() }
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
