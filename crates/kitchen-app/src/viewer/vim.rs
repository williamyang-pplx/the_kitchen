//! Vim normal/visual-mode navigation over a read-only buffer.
//!
//! Pure state machine: feed it [`VimKey`]s plus the buffer lines and the
//! viewport, it moves the cursor and returns [`VimAction`]s for things the
//! owner must do (jump to a definition, scroll, switch file). No egui here,
//! so every motion is unit-testable.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimKey {
    Char(char),
    Ctrl(char),
    Esc,
    Enter,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VimAction {
    /// gd / Ctrl-]: jump to the definition of this identifier.
    JumpToDef(String),
    /// gr: list references to this identifier.
    References(String),
    /// Ctrl-o / Ctrl-t.
    JumpBack,
    /// Ctrl-i.
    JumpForward,
    /// Enter (outline: open the symbol).
    Enter,
    /// ]c / [c
    NextHunk,
    PrevHunk,
    /// ]f / [f
    NextFile,
    PrevFile,
    /// Scroll the viewport by this many lines (Ctrl-d/u/f/b, wheel).
    ScrollBy(isize),
    /// zz / zt / zb
    Center,
    Top,
    Bottom,
    /// zo: open the fold under the cursor (Enter on a fold row does too).
    OpenFold,
    /// zR / zM: open every fold in the file / fold the file again.
    OpenAllFolds,
    CloseAllFolds,
    /// The cursor moved far enough to be worth recording in the jump list
    /// (gg, G, search, %, {, }).
    BigMove { from: (usize, usize) },
    /// Esc with nothing to cancel.
    Escape,
    /// `-`: the folder view one level up (from a file: its folder).
    Parent,
    /// ]d / [d: next / previous plugin diagnostic.
    NextDiag,
    PrevDiag,
}

/// What the owner tells the machine about the viewport.
#[derive(Clone, Copy, Debug, Default)]
pub struct Viewport {
    pub top: usize,
    pub rows: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FindKind {
    F,
    BigF,
    T,
    BigT,
}

#[derive(Clone, Debug, Default)]
pub struct Vim {
    pub line: usize,
    pub col: usize,
    /// Column `j`/`k` try to return to.
    want_col: usize,
    /// Visual mode anchor (line, col, linewise).
    pub anchor: Option<(usize, usize, bool)>,
    count: String,
    pending: Option<char>,
    last_find: Option<(FindKind, char)>,
    /// Last search (pattern, forward).
    pub search: Option<(String, bool)>,
    /// `/` or `?` prompt being typed.
    pub search_input: Option<(String, bool)>,
    /// Short status message (e.g. "pattern not found").
    pub message: Option<String>,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Class {
    Space,
    Word,
    Punct,
}

fn class(c: char, big: bool) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if big || c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

fn chars(line: &str) -> Vec<char> {
    line.chars().collect()
}

impl Vim {
    pub fn set_cursor(&mut self, line: usize, col: usize) {
        self.line = line;
        self.col = col;
        self.want_col = col;
    }

    /// Visual selection as an inclusive line range, if active.
    pub fn visual_lines(&self) -> Option<(usize, usize)> {
        self.anchor.map(|(l, _, _)| (l.min(self.line), l.max(self.line)))
    }

    /// Pending count/operator keys, for the status line.
    pub fn pending_display(&self) -> String {
        format!("{}{}", self.count, self.pending.map(String::from).unwrap_or_default())
    }

    fn take_count(&mut self) -> Option<usize> {
        let c = self.count.parse().ok();
        self.count.clear();
        c
    }

    fn clamp(&mut self, lines: &[String]) {
        if lines.is_empty() {
            self.line = 0;
            self.col = 0;
            return;
        }
        self.line = self.line.min(lines.len() - 1);
        let len = lines[self.line].chars().count();
        self.col = self.col.min(len.saturating_sub(1));
    }

    /// Identifier under (or right after) the cursor.
    pub fn word_under_cursor(&self, lines: &[String]) -> Option<String> {
        word_at(lines.get(self.line)?, self.col)
    }

    /// Feed one key. Returns the actions the owner must perform.
    pub fn key(&mut self, key: VimKey, lines: &[String], view: Viewport) -> Vec<VimAction> {
        self.message = None;
        if let Some((mut q, fwd)) = self.search_input.take() {
            match key {
                VimKey::Esc => {}
                VimKey::Enter => {
                    let q = if q.is_empty() { self.search.as_ref().map(|s| s.0.clone()).unwrap_or_default() } else { q };
                    if !q.is_empty() {
                        self.search = Some((q, fwd));
                        return self.search_next(lines, false);
                    }
                }
                VimKey::Backspace => {
                    if q.pop().is_some() || !q.is_empty() {
                        self.search_input = Some((q, fwd));
                    }
                }
                VimKey::Char(c) => {
                    q.push(c);
                    self.search_input = Some((q, fwd));
                }
                _ => self.search_input = Some((q, fwd)),
            }
            return Vec::new();
        }
        let out = self.key_inner(key, lines, view);
        self.clamp(lines);
        out
    }

    fn key_inner(&mut self, key: VimKey, lines: &[String], view: Viewport) -> Vec<VimAction> {
        let n_lines = lines.len();
        if n_lines == 0 {
            return Vec::new();
        }
        if let Some(p) = self.pending.take() {
            return self.pending_key(p, key, lines);
        }
        let from = (self.line, self.col);
        let half = (view.rows / 2).max(1);
        match key {
            VimKey::Char(c @ '1'..='9') => {
                self.count.push(c);
                return Vec::new();
            }
            VimKey::Char('0') if !self.count.is_empty() => {
                self.count.push('0');
                return Vec::new();
            }
            _ => {}
        }
        let count = self.take_count();
        let n = count.unwrap_or(1);
        let mut out = Vec::new();
        match key {
            VimKey::Char('j') | VimKey::Down => self.vertical(n as isize, lines),
            VimKey::Enter => out.push(VimAction::Enter),
            VimKey::Char('-') => out.push(VimAction::Parent),
            VimKey::Char('k') | VimKey::Up => self.vertical(-(n as isize), lines),
            VimKey::Char('h') | VimKey::Left | VimKey::Backspace => {
                self.col = self.col.saturating_sub(n);
                self.want_col = self.col;
            }
            VimKey::Char('l') | VimKey::Right | VimKey::Char(' ') => {
                let len = lines[self.line].chars().count();
                self.col = (self.col + n).min(len.saturating_sub(1));
                self.want_col = self.col;
            }
            VimKey::Char('0') | VimKey::Home => self.set_col(0),
            VimKey::Char('^') => {
                let c = lines[self.line].chars().take_while(|c| c.is_whitespace()).count();
                self.set_col(c);
            }
            VimKey::Char('$') | VimKey::End => {
                if n > 1 {
                    self.line = (self.line + n - 1).min(n_lines - 1);
                }
                let len = lines[self.line].chars().count();
                self.col = len.saturating_sub(1);
                self.want_col = usize::MAX;
            }
            VimKey::Char('w') => (0..n).for_each(|_| self.word_forward(lines, false)),
            VimKey::Char('W') => (0..n).for_each(|_| self.word_forward(lines, true)),
            VimKey::Char('b') => (0..n).for_each(|_| self.word_back(lines, false)),
            VimKey::Char('B') => (0..n).for_each(|_| self.word_back(lines, true)),
            VimKey::Char('e') => (0..n).for_each(|_| self.word_end(lines, false)),
            VimKey::Char('E') => (0..n).for_each(|_| self.word_end(lines, true)),
            VimKey::Char('G') => {
                let target = count.map(|c| c.saturating_sub(1)).unwrap_or(n_lines - 1);
                self.goto_line(target, lines);
                out.push(VimAction::BigMove { from });
            }
            VimKey::Char('H') => self.goto_line(view.top + n - 1, lines),
            VimKey::Char('M') => self.goto_line(view.top + view.rows.min(n_lines - view.top.min(n_lines)) / 2, lines),
            VimKey::Char('L') => {
                let bottom = (view.top + view.rows).min(n_lines).saturating_sub(1);
                self.goto_line(bottom.saturating_sub(n - 1), lines);
            }
            VimKey::Char('}') => {
                for _ in 0..n {
                    self.paragraph(lines, true);
                }
                out.push(VimAction::BigMove { from });
            }
            VimKey::Char('{') => {
                for _ in 0..n {
                    self.paragraph(lines, false);
                }
                out.push(VimAction::BigMove { from });
            }
            VimKey::Char('%') => {
                if self.match_bracket(lines) {
                    out.push(VimAction::BigMove { from });
                }
            }
            VimKey::Ctrl('d') => {
                self.vertical((half * n) as isize, lines);
                out.push(VimAction::ScrollBy((half * n) as isize));
            }
            VimKey::Ctrl('u') => {
                self.vertical(-((half * n) as isize), lines);
                out.push(VimAction::ScrollBy(-((half * n) as isize)));
            }
            VimKey::Ctrl('f') | VimKey::PageDown => {
                let page = view.rows.saturating_sub(2).max(1) * n;
                self.vertical(page as isize, lines);
                out.push(VimAction::ScrollBy(page as isize));
            }
            VimKey::Ctrl('b') | VimKey::PageUp => {
                let page = view.rows.saturating_sub(2).max(1) * n;
                self.vertical(-(page as isize), lines);
                out.push(VimAction::ScrollBy(-(page as isize)));
            }
            VimKey::Ctrl('e') => out.push(VimAction::ScrollBy(n as isize)),
            VimKey::Ctrl('y') => out.push(VimAction::ScrollBy(-(n as isize))),
            VimKey::Ctrl(']') => {
                if let Some(w) = self.word_under_cursor(lines) {
                    out.push(VimAction::JumpToDef(w));
                }
            }
            VimKey::Ctrl('o') | VimKey::Ctrl('t') => out.push(VimAction::JumpBack),
            VimKey::Ctrl('i') => out.push(VimAction::JumpForward),
            VimKey::Char('/') => self.search_input = Some((String::new(), true)),
            VimKey::Char('?') => self.search_input = Some((String::new(), false)),
            VimKey::Char('n') => (0..n).for_each(|_| out.extend(self.search_next(lines, false))),
            VimKey::Char('N') => (0..n).for_each(|_| out.extend(self.search_next(lines, true))),
            VimKey::Char(c @ ('*' | '#')) => {
                if let Some(w) = self.word_under_cursor(lines) {
                    self.search = Some((format!("\\b{w}\\b"), c == '*'));
                    out.extend(self.search_next(lines, false));
                }
            }
            VimKey::Char(';') => self.repeat_find(lines, false, n),
            VimKey::Char(',') => self.repeat_find(lines, true, n),
            VimKey::Char('v') => self.toggle_visual(false),
            VimKey::Char('V') => self.toggle_visual(true),
            VimKey::Esc => {
                if self.anchor.take().is_none() {
                    out.push(VimAction::Escape);
                }
            }
            VimKey::Char(c @ ('g' | 'z' | '[' | ']' | 'f' | 'F' | 't' | 'T')) => {
                if let Some(c) = count {
                    self.count = c.to_string();
                }
                self.pending = Some(c);
            }
            _ => {}
        }
        out
    }

    fn pending_key(&mut self, p: char, key: VimKey, lines: &[String]) -> Vec<VimAction> {
        let from = (self.line, self.col);
        let count = self.take_count();
        let VimKey::Char(c) = key else {
            return Vec::new();
        };
        match (p, c) {
            ('g', 'g') => {
                self.goto_line(count.map(|n| n.saturating_sub(1)).unwrap_or(0), lines);
                vec![VimAction::BigMove { from }]
            }
            ('g', 'd') => self.word_under_cursor(lines).map(VimAction::JumpToDef).into_iter().collect(),
            ('g', 'r') => self.word_under_cursor(lines).map(VimAction::References).into_iter().collect(),
            ('z', 'z') => vec![VimAction::Center],
            ('z', 't') => vec![VimAction::Top],
            ('z', 'b') => vec![VimAction::Bottom],
            ('z', 'o') => vec![VimAction::OpenFold],
            ('z', 'R') => vec![VimAction::OpenAllFolds],
            ('z', 'M') => vec![VimAction::CloseAllFolds],
            (']', 'c') => vec![VimAction::NextHunk],
            (']', 'd') => vec![VimAction::NextDiag],
            ('[', 'd') => vec![VimAction::PrevDiag],
            ('[', 'c') => vec![VimAction::PrevHunk],
            (']', 'f') => vec![VimAction::NextFile],
            ('[', 'f') => vec![VimAction::PrevFile],
            ('f' | 'F' | 't' | 'T', ch) => {
                let kind = match p {
                    'f' => FindKind::F,
                    'F' => FindKind::BigF,
                    't' => FindKind::T,
                    _ => FindKind::BigT,
                };
                self.last_find = Some((kind, ch));
                for _ in 0..count.unwrap_or(1) {
                    self.find(lines, kind, ch, false);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn set_col(&mut self, c: usize) {
        self.col = c;
        self.want_col = c;
    }

    fn toggle_visual(&mut self, linewise: bool) {
        match self.anchor {
            Some((_, _, lw)) if lw == linewise => self.anchor = None,
            Some((l, c, _)) => self.anchor = Some((l, c, linewise)),
            None => self.anchor = Some((self.line, self.col, linewise)),
        }
    }

    fn vertical(&mut self, delta: isize, lines: &[String]) {
        let target = (self.line as isize + delta).clamp(0, lines.len() as isize - 1) as usize;
        self.line = target;
        let len = lines[target].chars().count();
        self.col = self.want_col.min(len.saturating_sub(1));
    }

    fn goto_line(&mut self, line: usize, lines: &[String]) {
        self.line = line.min(lines.len() - 1);
        let first = lines[self.line].chars().take_while(|c| c.is_whitespace()).count();
        self.set_col(first);
    }

    /// Next position (line, col) after the cursor, crossing lines; empty
    /// lines count as a single position.
    fn step(&self, lines: &[String], pos: (usize, usize), fwd: bool) -> Option<(usize, usize)> {
        let (l, c) = pos;
        if fwd {
            let len = lines[l].chars().count();
            if c + 1 < len {
                Some((l, c + 1))
            } else if l + 1 < lines.len() {
                Some((l + 1, 0))
            } else {
                None
            }
        } else if c > 0 {
            Some((l, c - 1))
        } else if l > 0 {
            Some((l - 1, lines[l - 1].chars().count().saturating_sub(1)))
        } else {
            None
        }
    }

    fn char_at(lines: &[String], (l, c): (usize, usize)) -> char {
        // Empty lines (and line ends) behave like whitespace boundaries.
        lines[l].chars().nth(c).unwrap_or('\n')
    }

    fn word_forward(&mut self, lines: &[String], big: bool) {
        let mut pos = (self.line, self.col);
        let start_class = class(Self::char_at(lines, pos), big);
        let start_line = pos.0;
        // Skip the rest of the current word.
        while let Some(next) = self.step(lines, pos, true) {
            let crossed = next.0 != pos.0;
            pos = next;
            if crossed || class(Self::char_at(lines, pos), big) != start_class {
                break;
            }
        }
        // Skip whitespace (an empty line is a stop).
        loop {
            let ch = Self::char_at(lines, pos);
            if lines[pos.0].is_empty() && pos.0 != start_line {
                break;
            }
            if class(ch, big) != Class::Space {
                break;
            }
            match self.step(lines, pos, true) {
                Some(n) => pos = n,
                None => break,
            }
        }
        self.line = pos.0;
        self.set_col(pos.1);
    }

    fn word_back(&mut self, lines: &[String], big: bool) {
        let mut pos = (self.line, self.col);
        // Step back once, then over whitespace.
        let Some(p) = self.step(lines, pos, false) else { return };
        pos = p;
        while class(Self::char_at(lines, pos), big) == Class::Space && !lines[pos.0].is_empty() {
            match self.step(lines, pos, false) {
                Some(p) => pos = p,
                None => break,
            }
        }
        let cls = class(Self::char_at(lines, pos), big);
        while pos.1 > 0 {
            let prev = (pos.0, pos.1 - 1);
            if class(Self::char_at(lines, prev), big) != cls {
                break;
            }
            pos = prev;
        }
        self.line = pos.0;
        self.set_col(pos.1);
    }

    fn word_end(&mut self, lines: &[String], big: bool) {
        let mut pos = (self.line, self.col);
        let Some(p) = self.step(lines, pos, true) else { return };
        pos = p;
        while class(Self::char_at(lines, pos), big) == Class::Space {
            match self.step(lines, pos, true) {
                Some(p) => pos = p,
                None => break,
            }
        }
        let cls = class(Self::char_at(lines, pos), big);
        let len = lines[pos.0].chars().count();
        while pos.1 + 1 < len && class(Self::char_at(lines, (pos.0, pos.1 + 1)), big) == cls {
            pos.1 += 1;
        }
        self.line = pos.0;
        self.set_col(pos.1);
    }

    fn paragraph(&mut self, lines: &[String], fwd: bool) {
        let blank = |i: usize| lines[i].trim().is_empty();
        let mut i = self.line;
        if fwd {
            while i + 1 < lines.len() && blank(i) {
                i += 1;
            }
            while i + 1 < lines.len() && !blank(i) {
                i += 1;
            }
        } else {
            while i > 0 && blank(i) {
                i -= 1;
            }
            while i > 0 && !blank(i) {
                i -= 1;
            }
        }
        self.line = i;
        self.set_col(0);
    }

    fn match_bracket(&mut self, lines: &[String]) -> bool {
        let pairs = [('(', ')'), ('[', ']'), ('{', '}')];
        let cs = chars(&lines[self.line]);
        // Like vim: use the first bracket at or after the cursor on this line.
        let Some(start) = (self.col..cs.len()).find(|&i| pairs.iter().any(|(o, c)| cs[i] == *o || cs[i] == *c)) else {
            return false;
        };
        let ch = cs[start];
        let (open, close, fwd) = pairs
            .iter()
            .find_map(|&(o, c)| if ch == o { Some((o, c, true)) } else if ch == c { Some((o, c, false)) } else { None })
            .expect("bracket");
        let mut depth = 0i32;
        let mut pos = (self.line, start);
        loop {
            let c = Self::char_at(lines, pos);
            if c == open {
                depth += if fwd { 1 } else { -1 };
            } else if c == close {
                depth += if fwd { -1 } else { 1 };
            }
            if depth == 0 {
                self.line = pos.0;
                self.set_col(pos.1);
                return true;
            }
            match self.step(lines, pos, fwd) {
                Some(p) => pos = p,
                None => return false,
            }
        }
    }

    fn find(&mut self, lines: &[String], kind: FindKind, ch: char, repeat: bool) {
        let cs = chars(&lines[self.line]);
        let col = self.col;
        let target = match kind {
            FindKind::F => (col + 1..cs.len()).find(|&i| cs[i] == ch),
            FindKind::T => {
                let from = if repeat { col + 2 } else { col + 1 };
                (from..cs.len()).find(|&i| cs[i] == ch).map(|i| i - 1)
            }
            FindKind::BigF => (0..col).rev().find(|&i| cs[i] == ch),
            FindKind::BigT => {
                let upto = if repeat { col.saturating_sub(1) } else { col };
                (0..upto).rev().find(|&i| cs[i] == ch).map(|i| i + 1)
            }
        };
        if let Some(t) = target {
            self.set_col(t);
        }
    }

    fn repeat_find(&mut self, lines: &[String], reverse: bool, n: usize) {
        let Some((kind, ch)) = self.last_find else { return };
        let kind = if !reverse {
            kind
        } else {
            match kind {
                FindKind::F => FindKind::BigF,
                FindKind::BigF => FindKind::F,
                FindKind::T => FindKind::BigT,
                FindKind::BigT => FindKind::T,
            }
        };
        for _ in 0..n {
            self.find(lines, kind, ch, true);
        }
    }

    /// Jump to the next match of the last search (`reverse` flips direction).
    fn search_next(&mut self, lines: &[String], reverse: bool) -> Vec<VimAction> {
        let Some((pat, fwd)) = self.search.clone() else { return Vec::new() };
        let fwd = fwd != reverse;
        let smart_case = !pat.chars().any(|c| c.is_uppercase());
        let re = regex::RegexBuilder::new(&pat)
            .case_insensitive(smart_case)
            .build()
            .or_else(|_| regex::RegexBuilder::new(&regex::escape(&pat)).case_insensitive(smart_case).build());
        let Ok(re) = re else { return Vec::new() };
        let from = (self.line, self.col);
        let n = lines.len();
        // Byte offset <-> char col helpers per line.
        let col_of = |line: &str, byte: usize| line[..byte].chars().count();
        for step in 0..=n {
            let idx = if fwd { (self.line + step) % n } else { (self.line + n - step % n) % n };
            let line = &lines[idx];
            let matches: Vec<usize> = re.find_iter(line).map(|m| col_of(line, m.start())).collect();
            let hit = if fwd {
                matches.into_iter().find(|&c| step > 0 || c > self.col)
            } else {
                matches.into_iter().rev().find(|&c| step > 0 || c < self.col)
            };
            if let Some(c) = hit {
                if step > 0 && ((fwd && idx < self.line) || (!fwd && idx > self.line)) {
                    self.message = Some(if fwd { "search hit BOTTOM, continuing at TOP".into() } else { "search hit TOP, continuing at BOTTOM".into() });
                }
                self.line = idx;
                self.set_col(c);
                return vec![VimAction::BigMove { from }];
            }
        }
        self.message = Some(format!("pattern not found: {pat}"));
        Vec::new()
    }
}

/// The identifier at (or right after) display column `col` of `line`.
pub fn word_at(line: &str, col: usize) -> Option<String> {
    let cs = chars(line);
    if cs.is_empty() {
        return None;
    }
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut i = col.min(cs.len() - 1);
    if !is_w(cs[i]) {
        i = (i..cs.len()).find(|&j| is_w(cs[j]))?;
    }
    let start = (0..=i).rev().take_while(|&j| is_w(cs[j])).last().unwrap_or(i);
    let end = (i..cs.len()).take_while(|&j| is_w(cs[j])).last().unwrap_or(i);
    Some(cs[start..=end].iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    fn keys(v: &mut Vim, lines: &[String], ks: &str) -> Vec<VimAction> {
        let view = Viewport { top: 0, rows: 10 };
        let mut out = Vec::new();
        for c in ks.chars() {
            out.extend(v.key(VimKey::Char(c), lines, view));
        }
        out
    }

    #[test]
    fn basic_motions() {
        let b = buf("fn main() {\n    let x = foo(1);\n}\n");
        let mut v = Vim::default();
        keys(&mut v, &b, "j");
        assert_eq!((v.line, v.col), (1, 0));
        keys(&mut v, &b, "^");
        assert_eq!(v.col, 4);
        keys(&mut v, &b, "$");
        assert_eq!(v.col, 18);
        keys(&mut v, &b, "k");
        assert_eq!(v.col, 10, "$ sticks to end of line");
        keys(&mut v, &b, "0");
        assert_eq!(v.col, 0);
        keys(&mut v, &b, "G");
        assert_eq!(v.line, 2);
        keys(&mut v, &b, "gg");
        assert_eq!(v.line, 0);
        keys(&mut v, &b, "2G");
        assert_eq!(v.line, 1);
    }

    #[test]
    fn word_motions() {
        let b = buf("let foo_bar = baz(1);\n\nnext");
        let mut v = Vim::default();
        keys(&mut v, &b, "w");
        assert_eq!(v.col, 4);
        keys(&mut v, &b, "w");
        assert_eq!(v.col, 12);
        keys(&mut v, &b, "e");
        assert_eq!(v.col, 16, "e from a one-char word goes to the next word's end");
        keys(&mut v, &b, "e");
        assert_eq!(v.col, 17, "punctuation run '(' is its own word");
        keys(&mut v, &b, "b");
        assert_eq!(v.col, 14);
        keys(&mut v, &b, "W");
        assert_eq!((v.line, v.col), (1, 0), "empty line is a word stop");
        keys(&mut v, &b, "w");
        assert_eq!((v.line, v.col), (2, 0));
        keys(&mut v, &b, "B");
        assert_eq!((v.line, v.col), (1, 0));
        let mut v = Vim::default();
        keys(&mut v, &b, "3w");
        assert_eq!(v.col, 14);
    }

    #[test]
    fn find_and_repeat() {
        let b = buf("a(b, c(d), e)");
        let mut v = Vim::default();
        keys(&mut v, &b, "f(");
        assert_eq!(v.col, 1);
        keys(&mut v, &b, ";");
        assert_eq!(v.col, 6);
        keys(&mut v, &b, ",");
        assert_eq!(v.col, 1);
        keys(&mut v, &b, "t)");
        assert_eq!(v.col, 7);
        keys(&mut v, &b, "0%");
        assert_eq!(v.col, 12, "% from first bracket on line");
        keys(&mut v, &b, "%");
        assert_eq!(v.col, 1);
        keys(&mut v, &b, "$F,");
        assert_eq!(v.col, 9);
    }

    #[test]
    fn search() {
        let b = buf("alpha\nbeta Alpha\ngamma alpha");
        let mut v = Vim::default();
        let view = Viewport { top: 0, rows: 10 };
        v.key(VimKey::Char('/'), &b, view);
        for c in "alpha".chars() {
            v.key(VimKey::Char(c), &b, view);
        }
        let acts = v.key(VimKey::Enter, &b, view);
        assert!(matches!(acts[0], VimAction::BigMove { .. }));
        assert_eq!((v.line, v.col), (1, 5), "smart-case matches Alpha");
        keys(&mut v, &b, "n");
        assert_eq!((v.line, v.col), (2, 6));
        keys(&mut v, &b, "n");
        assert_eq!((v.line, v.col), (0, 0), "wraps");
        assert!(v.message.is_some());
        keys(&mut v, &b, "N");
        assert_eq!((v.line, v.col), (2, 6));
        let mut v = Vim::default();
        keys(&mut v, &b, "*");
        assert_eq!((v.line, v.col), (1, 5));
    }

    #[test]
    fn paragraphs_and_viewport() {
        let b = buf("a\nb\n\nc\nd\n\ne");
        let mut v = Vim::default();
        keys(&mut v, &b, "}");
        assert_eq!(v.line, 2);
        keys(&mut v, &b, "}");
        assert_eq!(v.line, 5);
        keys(&mut v, &b, "{");
        assert_eq!(v.line, 2);
        let view = Viewport { top: 2, rows: 3 };
        v.key(VimKey::Char('H'), &b, view);
        assert_eq!(v.line, 2);
        v.key(VimKey::Char('L'), &b, view);
        assert_eq!(v.line, 4);
        let acts = v.key(VimKey::Ctrl('d'), &b, Viewport { top: 0, rows: 4 });
        assert_eq!(acts, vec![VimAction::ScrollBy(2)]);
    }

    #[test]
    fn actions() {
        let b = buf("x = compute(y)");
        let mut v = Vim::default();
        keys(&mut v, &b, "fc");
        assert_eq!(keys(&mut v, &b, "gd"), vec![VimAction::JumpToDef("compute".into())]);
        assert_eq!(keys(&mut v, &b, "gr"), vec![VimAction::References("compute".into())]);
        assert_eq!(v.key(VimKey::Ctrl(']'), &b, Viewport::default()), vec![VimAction::JumpToDef("compute".into())]);
        assert_eq!(v.key(VimKey::Ctrl('o'), &b, Viewport::default()), vec![VimAction::JumpBack]);
        assert_eq!(keys(&mut v, &b, "]c[f"), vec![VimAction::NextHunk, VimAction::PrevFile]);
        assert_eq!(keys(&mut v, &b, "zz"), vec![VimAction::Center]);
        let at = (v.line, v.col);
        assert_eq!(keys(&mut v, &b, "zozRzM"), vec![VimAction::OpenFold, VimAction::OpenAllFolds, VimAction::CloseAllFolds]);
        assert_eq!((v.line, v.col), at, "fold keys don't move the cursor");
        keys(&mut v, &b, "V");
        assert_eq!(v.visual_lines(), Some((0, 0)));
        assert!(v.key(VimKey::Esc, &b, Viewport::default()).is_empty());
        assert_eq!(v.visual_lines(), None);
        assert_eq!(v.key(VimKey::Esc, &b, Viewport::default()), vec![VimAction::Escape]);
    }
}
