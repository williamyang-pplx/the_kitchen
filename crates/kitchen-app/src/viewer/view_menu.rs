//! ⌘⇧S: the PR menu. Its rows are what plugins gave the tab
//! (`kitchen.views`: the PRs its agent listed, one section per stack), then
//! the tab's own worktree. Picking a row hands its view to the window
//! (`PanelCtx::view`), which shows it from its viewer cache or checks it out.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::picker::Picker;
use super::{PickerKind, Viewer};
use crate::engine::{MenuRow, Shown, View, ViewAt};

/// How long the menu says "loading PRs…" while it waits for a plugin.
const ASK_TIMEOUT: Duration = Duration::from_secs(10);

/// The menu's picker items, section headers, each row's view, and the row
/// on screen.
struct Rows {
    items: Vec<(String, String)>,
    sections: Vec<(usize, String)>,
    views: Vec<(View, Shown)>,
    current: Option<usize>,
}

impl Viewer {
    /// (worktree, diff base): what this viewer shows, its cache key.
    pub fn shown(&self) -> Shown {
        (self.worktree.clone(), self.base_ref.clone())
    }

    /// The open file, the cursor's source line and the diff mode, so a
    /// rebuilt viewer opens where this one was.
    pub fn location(&self) -> (Option<String>, Option<usize>, bool) {
        (self.current.clone().filter(|_| !self.browsing), self.current_source_line(), self.diff)
    }

    fn view_menu_rows(&self, menu: Option<&Arc<Vec<MenuRow>>>) -> Rows {
        let shown = self.shown();
        let mut rows = Rows { items: Vec::new(), sections: Vec::new(), views: Vec::new(), current: None };
        let push = |rows: &mut Rows, section: &str, label: &str, detail: &str, view: View, at: Shown| {
            if rows.sections.last().is_none_or(|(_, s)| s != section) {
                rows.sections.push((rows.items.len(), section.to_string()));
            }
            let current = at == shown;
            if current {
                rows.current = Some(rows.items.len());
            }
            rows.items.push((format!("{}{label}", if current { "● " } else { "" }), detail.to_string()));
            rows.views.push((view, at));
        };
        for (e, at) in menu.map(|m| m.as_slice()).unwrap_or_default() {
            push(&mut rows, &e.section, &e.label, &e.detail, View { tab: self.tab_id.clone(), at: e.view.clone() }, at.clone());
        }
        // The tab's worktree against its diff base, unless a row already is.
        if let Some((ws, tab)) = self.engine.find_tab(&self.tab_id) {
            let own = (tab.row.worktree.clone(), tab.diff_base(&ws));
            if !rows.views.iter().any(|(_, at)| *at == own) {
                let detail = format!("{} vs {}", tab.row.branch.as_deref().unwrap_or("HEAD"), own.1);
                push(&mut rows, "This tab", "worktree", &detail, View { tab: self.tab_id.clone(), at: ViewAt::default() }, own);
            }
        }
        rows
    }

    /// ⌘⇧S: open the PR menu (or close it), asking the plugins to refresh it.
    pub(super) fn open_view_menu(&mut self) {
        if matches!(self.picker, Some((PickerKind::Views { .. }, _))) {
            self.picker = None;
            return;
        }
        self.asked = self.engine.ask_views(&self.tab_id, self.context()).then(Instant::now);
        let menu = self.engine.views_of(&self.tab_id);
        let rows = self.view_menu_rows(menu.as_ref());
        let mut picker = Picker::menu("PRs", rows.items, rows.sections);
        picker.hint = "The PRs this tab's agent listed, one section per stack".into();
        picker.loading_note = "loading PRs…".into();
        picker.loading = self.asked.is_some();
        if let Some(i) = rows.current {
            picker.select(i);
        }
        self.picker = Some((PickerKind::Views { menu, views: rows.views }, picker));
    }

    /// An open menu follows `kitchen.views` (keeping a cursor the user
    /// moved on its row), and stops saying "loading" once a plugin answered
    /// or `ASK_TIMEOUT` passed.
    pub(super) fn refresh_view_menu(&mut self, ctx: &egui::Context) {
        if !matches!(self.picker, Some((PickerKind::Views { .. }, _))) {
            return;
        }
        let latest = self.engine.views_of(&self.tab_id);
        let Some((PickerKind::Views { menu, .. }, _)) = &self.picker else { return };
        let changed = match (menu, &latest) {
            (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
            (a, b) => a.is_some() != b.is_some(),
        };
        if changed {
            self.asked = None;
            let rows = self.view_menu_rows(latest.as_ref());
            if let Some((PickerKind::Views { menu, views }, picker)) = &mut self.picker {
                // A cursor the user moved stays on its row (the menu may have
                // arrived while they were moving); otherwise it follows the
                // row on screen.
                let keep = if picker.moved {
                    let at = picker.selected_item().and_then(|i| views.get(i)).map(|(_, at)| at);
                    at.and_then(|at| rows.views.iter().position(|(_, a)| a == at))
                } else {
                    rows.current
                };
                picker.set_menu(rows.items, rows.sections);
                if let Some(i) = keep {
                    picker.select(i);
                }
                *menu = latest;
                *views = rows.views;
            }
        }
        let left = self.asked.map(|t| ASK_TIMEOUT.saturating_sub(t.elapsed())).filter(|d| !d.is_zero());
        if let Some(d) = left {
            ctx.request_repaint_after(d);
        }
        if let Some((_, picker)) = &mut self.picker {
            picker.loading = left.is_some();
        }
    }

    /// The header button for the row on screen: "#123 · 2/3" (its place in
    /// its section), when the tab's menu has that row.
    pub(super) fn shown_entry_label(&self) -> Option<String> {
        let menu = self.engine.views_of(&self.tab_id)?;
        let shown = self.shown();
        let (e, _) = menu.iter().find(|(_, at)| *at == shown)?;
        let section: Vec<_> = menu.iter().filter(|(o, _)| o.section == e.section).collect();
        let at = section.iter().position(|(o, s)| *s == shown && o == e)? + 1;
        let name = e.label.split_whitespace().next().unwrap_or(&e.label);
        Some(if section.len() > 1 { format!("{name} · {at}/{}", section.len()) } else { name.to_string() })
    }
}

#[cfg(test)]
impl Viewer {
    /// The PR menu is up.
    pub fn menu_open(&self) -> bool {
        matches!(self.picker, Some((PickerKind::Views { .. }, _)))
    }
}

#[cfg(test)]
mod tests {
    use kitchen_core::host::Host;

    use super::super::PickerKind;
    use super::*;
    use crate::engine::ViewSet;
    use crate::panels::PanelCmd;
    use crate::test_util::{Harness, TestRepo};

    fn set(h: &Harness, entries: serde_json::Value) {
        let set: ViewSet = serde_json::from_value(serde_json::json!({ "tab": "t", "entries": entries })).unwrap();
        h.engine.set_views(set).unwrap();
    }

    fn rows(v: &Viewer) -> Vec<String> {
        match &v.picker {
            Some((PickerKind::Views { .. }, p)) => p.items.iter().map(|(l, _)| l.clone()).collect(),
            _ => panic!("no PR menu"),
        }
    }

    /// ⌘⇧S lists the tab's entries and its worktree, with the cursor on what
    /// is shown; it follows a new menu; Enter hands the row's view to the
    /// window; ⌘⇧S again closes it.
    #[test]
    fn view_menu_lists_entries_and_marks_the_shown_one() {
        let h = Harness::new(TestRepo::new("echo 1 > a"));
        let stack = serde_json::json!([
            { "section": "main ← a", "label": "#7 a", "view": { "rev": "refs/kitchen/pr/7", "name": "kitchen-pr-7", "base": "origin/main" } },
            { "section": "main ← a", "label": "#8 b", "view": { "base": "origin/a" } },
        ]);
        set(&h, stack.clone());
        let mut v = h.viewer();
        v.command(PanelCmd::ViewMenu, &h.engine);
        h.frame(&mut v);
        // The worktree row stays: the live row diffs against origin/a, the viewer against origin/main.
        assert_eq!(rows(&v), vec!["#7 a", "#8 b", "● worktree"]);
        let Some((_, p)) = &v.picker else { unreachable!() };
        assert_eq!(p.filtered[p.selected].0, 2, "cursor on the row shown");

        let mut more = stack.as_array().unwrap().clone();
        more.push(serde_json::json!({ "section": "Reviewing", "label": "#9 c", "view": { "rev": "refs/kitchen/pr/9", "name": "kitchen-pr-9" } }));
        set(&h, serde_json::Value::Array(more));
        h.frame(&mut v);
        assert_eq!(rows(&v), vec!["#7 a", "#8 b", "#9 c", "● worktree"], "an open menu follows kitchen.views");

        for _ in 0..3 {
            h.frame_with(&mut v, vec![key(egui::Key::K)]);
        }
        let picked = h.frame_with(&mut v, vec![key(egui::Key::Enter)]).expect("a view for the window");
        assert_eq!(picked.at.name.as_deref(), Some("kitchen-pr-7"));
        assert!(!v.menu_open());
        v.command(PanelCmd::ViewMenu, &h.engine);
        assert!(v.menu_open());
        v.command(PanelCmd::ViewMenu, &h.engine);
        assert!(!v.menu_open(), "⌘⇧S again closes it");
    }

    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    fn cursor(v: &Viewer) -> String {
        match &v.picker {
            Some((PickerKind::Views { .. }, p)) => p.items[p.selected_item().unwrap()].0.clone(),
            _ => panic!("no PR menu"),
        }
    }

    /// The plugin's menu arrives a second or two after ⌘⇧S: a cursor the
    /// user already moved stays on its row (by what it shows, though the
    /// rows moved); one they didn't move goes to the row on screen.
    #[test]
    fn a_late_menu_keeps_the_cursor_the_user_moved() {
        let h = Harness::new(TestRepo::new("echo 1 > a"));
        let pr = |n: u32| serde_json::json!({ "section": "main ← a", "label": format!("#{n}"), "view": { "rev": format!("refs/kitchen/pr/{n}"), "name": format!("kitchen-pr-{n}") } });
        set(&h, serde_json::json!([pr(7), pr(8)]));
        let mut v = h.viewer();
        v.command(PanelCmd::ViewMenu, &h.engine);
        h.frame(&mut v);
        assert_eq!(cursor(&v), "● worktree");
        h.frame_with(&mut v, vec![key(egui::Key::K)]);
        assert_eq!(cursor(&v), "#8");
        set(&h, serde_json::json!([pr(6), pr(7), pr(8)]));
        h.frame(&mut v);
        assert_eq!(cursor(&v), "#8", "the plugin's answer doesn't take the cursor back");

        v.command(PanelCmd::ViewMenu, &h.engine);
        v.command(PanelCmd::ViewMenu, &h.engine);
        h.frame(&mut v);
        set(&h, serde_json::json!([pr(7)]));
        h.frame(&mut v);
        assert_eq!(cursor(&v), "● worktree", "an unmoved cursor follows the row on screen");
    }

    /// Picking the row on screen says it is on screen, rather than closing
    /// as if a switch had failed.
    #[test]
    fn picking_the_shown_row_says_so() {
        let h = Harness::new(TestRepo::new("echo 1 > a"));
        let mut v = h.viewer();
        v.command(PanelCmd::ViewMenu, &h.engine);
        h.frame(&mut v);
        assert!(h.frame_with(&mut v, vec![key(egui::Key::Enter)]).is_none(), "nothing to switch to");
        assert!(!v.menu_open());
        assert_eq!(v.status.as_ref().map(|(s, _)| s.as_str()), Some("this view is already on screen"));
    }

    /// The header names the row on screen and its place in its stack.
    #[test]
    fn header_label_names_the_shown_row() {
        let h = Harness::new(TestRepo::new("echo 1 > a"));
        set(
            &h,
            serde_json::json!([
                { "section": "main ← a", "label": "#7 a", "view": { "rev": "refs/kitchen/pr/7", "name": "kitchen-pr-7", "base": "origin/main" } },
                { "section": "main ← a", "label": "#8 b", "view": { "base": "origin/a" } },
                { "section": "main ← c", "label": "#9 c", "view": { "base": "origin/c" } },
            ]),
        );
        let label = |base: &str| Viewer::at(&h.engine, Host::Local, "t", h.repo.path(), base.into()).shown_entry_label();
        assert_eq!(label("origin/a").as_deref(), Some("#8 · 2/2"));
        assert_eq!(label("origin/c").as_deref(), Some("#9"), "a stack of one");
        assert_eq!(label("origin/main"), None, "not a row of the menu");
    }
}
