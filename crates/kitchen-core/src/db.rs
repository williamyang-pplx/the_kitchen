//! SQLite state (`~/.kitchen/kitchen.db` on the mac). Only the app process
//! opens it; agents reach it through the socket API.

use std::path::Path;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::util::now;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS workspaces (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    host           TEXT NOT NULL,          -- 'local' or ssh alias
    repo_path      TEXT NOT NULL,          -- absolute, on that host
    github_slug    TEXT,
    default_branch TEXT,
    position       INTEGER NOT NULL,
    pinned         INTEGER NOT NULL DEFAULT 0,  -- pinned workspaces sort to the top of the sidebar
    created_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tabs (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('edit', 'readonly')),
    title        TEXT NOT NULL,
    worktree     TEXT NOT NULL,
    branch       TEXT,
    tmux_session TEXT NOT NULL UNIQUE,
    plan_file    TEXT,
    diff_base    TEXT,
    position     INTEGER NOT NULL,
    created_at   INTEGER NOT NULL
);

-- Inline comments on a tab's plan document.
CREATE TABLE IF NOT EXISTS plan_comments (
    id          INTEGER PRIMARY KEY,
    tab_id      TEXT NOT NULL REFERENCES tabs(id) ON DELETE CASCADE,
    line        INTEGER NOT NULL,          -- 1-based line in the plan file
    end_line    INTEGER,
    quote       TEXT NOT NULL,             -- the line text when commented, to re-anchor after edits
    body        TEXT NOT NULL,
    state       TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'sent', 'resolved')),
    created_at  INTEGER NOT NULL,
    sent_at     INTEGER,
    resolved_at INTEGER
);

-- Model-written summaries of functions/classes, keyed by the symbol's
-- source hash so an edit invalidates exactly the symbols it touched.
CREATE TABLE IF NOT EXISTS summaries (
    symbol       TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    path         TEXT NOT NULL,
    summary      TEXT NOT NULL,
    model        TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (symbol, content_hash)
);

CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS notifications (
    id           INTEGER PRIMARY KEY,
    workspace_id TEXT,
    tab_id       TEXT,
    title        TEXT NOT NULL,
    body         TEXT NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    read         INTEGER NOT NULL DEFAULT 0
);

-- Plugin timers (`kitchen.every`): what each one's runs did, as JSON, so
-- runs and "last fired" survive restarts.
CREATE TABLE IF NOT EXISTS plugin_timers (
    plugin TEXT NOT NULL,
    name   TEXT NOT NULL,
    state  TEXT NOT NULL,
    PRIMARY KEY (plugin, name)
);
"#;

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceRow {
    pub id: String,
    pub name: String,
    pub host: String,
    pub repo_path: String,
    pub github_slug: Option<String>,
    pub default_branch: Option<String>,
    pub position: i64,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabRow {
    pub id: String,
    pub workspace_id: String,
    pub title: String,
    pub worktree: String,
    pub branch: Option<String>,
    pub tmux_session: String,
    pub position: i64,
    /// What the viewer diffs against (any rev); `None` = `origin/<default>`.
    #[serde(default)]
    pub diff_base: Option<String>,
}

impl TabRow {
    /// Where the tab's agent keeps its plan.
    pub fn plan_file(&self) -> String {
        crate::plan::plan_file_for(&self.worktree)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlanComment {
    pub id: i64,
    pub tab_id: String,
    pub line: i64,
    pub end_line: Option<i64>,
    pub quote: String,
    pub body: String,
    pub state: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Notification {
    pub id: i64,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub read: bool,
}

fn workspace_from(r: &Row) -> rusqlite::Result<WorkspaceRow> {
    Ok(WorkspaceRow {
        id: r.get(0)?,
        name: r.get(1)?,
        host: r.get(2)?,
        repo_path: r.get(3)?,
        github_slug: r.get(4)?,
        default_branch: r.get(5)?,
        position: r.get(6)?,
        pinned: r.get(7)?,
    })
}

fn tab_from(r: &Row) -> rusqlite::Result<TabRow> {
    Ok(TabRow {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        title: r.get(2)?,
        worktree: r.get(3)?,
        branch: r.get(4)?,
        tmux_session: r.get(5)?,
        position: r.get(6)?,
        diff_base: r.get(7)?,
    })
}

fn plan_comment_from(r: &Row) -> rusqlite::Result<PlanComment> {
    Ok(PlanComment {
        id: r.get(0)?,
        tab_id: r.get(1)?,
        line: r.get(2)?,
        end_line: r.get(3)?,
        quote: r.get(4)?,
        body: r.get(5)?,
        state: r.get(6)?,
        created_at: r.get(7)?,
    })
}

const WS_COLS: &str = "id, name, host, repo_path, github_slug, default_branch, position, pinned";
const TAB_COLS: &str = "id, workspace_id, title, worktree, branch, tmux_session, position, diff_base";
const PC_COLS: &str = "id, tab_id, line, end_line, quote, body, state, created_at";

/// Columns added after a table first shipped; `CREATE TABLE IF NOT EXISTS`
/// leaves older databases without them.
fn migrate(conn: &Connection) -> Result<()> {
    let has_pinned: bool =
        conn.query_row("SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name = 'pinned'", [], |r| r.get(0))?;
    if !has_pinned {
        conn.execute_batch("ALTER TABLE workspaces ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")?;
    }
    let has_base: bool = conn.query_row("SELECT COUNT(*) FROM pragma_table_info('tabs') WHERE name = 'diff_base'", [], |r| r.get(0))?;
    if !has_base {
        conn.execute_batch("ALTER TABLE tabs ADD COLUMN diff_base TEXT")?;
    }
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Db { conn })
    }

    // ------------------------------------------------------------ workspaces

    pub fn workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {WS_COLS} FROM workspaces ORDER BY pinned DESC, position, created_at"))?;
        Ok(st.query_map([], workspace_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_workspace(&self, w: &WorkspaceRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workspaces (id, name, host, repo_path, github_slug, default_branch, position, pinned, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![w.id, w.name, w.host, w.repo_path, w.github_slug, w.default_branch, w.position, w.pinned, now()],
        )?;
        Ok(())
    }

    pub fn update_workspace(&self, w: &WorkspaceRow) -> Result<()> {
        self.conn.execute(
            "UPDATE workspaces SET name = ?2, host = ?3, repo_path = ?4, github_slug = ?5, default_branch = ?6, position = ?7,
             pinned = ?8 WHERE id = ?1",
            params![w.id, w.name, w.host, w.repo_path, w.github_slug, w.default_branch, w.position, w.pinned],
        )?;
        Ok(())
    }

    /// Persist a new sidebar order: each workspace's position and pin state, atomically.
    pub fn reorder_workspaces(&self, order: &[(String, i64, bool)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (id, position, pinned) in order {
            tx.execute("UPDATE workspaces SET position = ?2, pinned = ?3 WHERE id = ?1", params![id, position, pinned])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_workspace(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM workspaces WHERE id = ?1", [id])?;
        Ok(())
    }

    // ------------------------------------------------------------ tabs

    pub fn tabs(&self, workspace_id: &str) -> Result<Vec<TabRow>> {
        let mut st =
            self.conn.prepare(&format!("SELECT {TAB_COLS} FROM tabs WHERE workspace_id = ?1 ORDER BY position, created_at"))?;
        Ok(st.query_map([workspace_id], tab_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn tab(&self, id: &str) -> Result<Option<TabRow>> {
        Ok(self.conn.query_row(&format!("SELECT {TAB_COLS} FROM tabs WHERE id = ?1"), [id], tab_from).optional()?)
    }

    pub fn insert_tab(&self, t: &TabRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tabs (id, workspace_id, kind, title, worktree, branch, tmux_session, position, diff_base, created_at)
             VALUES (?1, ?2, 'edit', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                t.id,
                t.workspace_id,
                t.title,
                t.worktree,
                t.branch,
                t.tmux_session,
                t.position,
                t.diff_base,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn update_tab(&self, t: &TabRow) -> Result<()> {
        self.conn.execute(
            "UPDATE tabs SET title = ?2, worktree = ?3, branch = ?4, position = ?5, diff_base = ?6 WHERE id = ?1",
            params![t.id, t.title, t.worktree, t.branch, t.position, t.diff_base],
        )?;
        Ok(())
    }

    pub fn delete_tab(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM tabs WHERE id = ?1", [id])?;
        Ok(())
    }

    // ------------------------------------------------------------ plan comments

    pub fn plan_comments(&self, tab_id: &str) -> Result<Vec<PlanComment>> {
        let mut st = self.conn.prepare(&format!("SELECT {PC_COLS} FROM plan_comments WHERE tab_id = ?1 ORDER BY line, id"))?;
        Ok(st.query_map([tab_id], plan_comment_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_plan_comment(&self, tab_id: &str, line: i64, end_line: Option<i64>, quote: &str, body: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO plan_comments (tab_id, line, end_line, quote, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![tab_id, line, end_line, quote, body, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn set_plan_comment_state(&self, id: i64, state: &str) -> Result<bool> {
        let col = match state {
            "sent" => "sent_at",
            "resolved" => "resolved_at",
            _ => "created_at",
        };
        let n = self.conn.execute(
            &format!("UPDATE plan_comments SET state = ?2, {col} = CASE WHEN ?2 = 'open' THEN {col} ELSE ?3 END WHERE id = ?1"),
            params![id, state, now()],
        )?;
        Ok(n > 0)
    }

    pub fn update_plan_comment_anchor(&self, id: i64, line: i64) -> Result<()> {
        self.conn.execute("UPDATE plan_comments SET line = ?2 WHERE id = ?1", params![id, line])?;
        Ok(())
    }

    // ------------------------------------------------------------ summaries

    pub fn summary(&self, symbol: &str, content_hash: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT summary FROM summaries WHERE symbol = ?1 AND content_hash = ?2",
                params![symbol, content_hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn put_summary(&self, symbol: &str, content_hash: &str, path: &str, summary: &str, model: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO summaries (symbol, content_hash, path, summary, model, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![symbol, content_hash, path, summary, model, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ settings

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }

    // ------------------------------------------------------------ plugin timers

    pub fn timer_state(&self, plugin: &str, name: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT state FROM plugin_timers WHERE plugin = ?1 AND name = ?2", params![plugin, name], |r| r.get(0)).optional()?)
    }

    pub fn put_timer_state(&self, plugin: &str, name: &str, state: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO plugin_timers (plugin, name, state) VALUES (?1, ?2, ?3)", params![plugin, name, state])?;
        Ok(())
    }

    // ------------------------------------------------------------ notifications

    pub fn add_notification(&self, ws: Option<&str>, tab: Option<&str>, title: &str, body: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notifications (workspace_id, tab_id, title, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![ws, tab, title, body, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn notifications(&self, unread_only: bool, limit: i64) -> Result<Vec<Notification>> {
        let sql = format!(
            "SELECT id, workspace_id, tab_id, title, body, created_at, read FROM notifications {} ORDER BY id DESC LIMIT ?1",
            if unread_only { "WHERE read = 0" } else { "" }
        );
        let mut st = self.conn.prepare(&sql)?;
        Ok(st
            .query_map([limit], |r| {
                Ok(Notification {
                    id: r.get(0)?,
                    workspace_id: r.get(1)?,
                    tab_id: r.get(2)?,
                    title: r.get(3)?,
                    body: r.get(4)?,
                    created_at: r.get(5)?,
                    read: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_notifications_read(&self, tab_id: Option<&str>) -> Result<()> {
        match tab_id {
            Some(t) => self.conn.execute("UPDATE notifications SET read = 1 WHERE tab_id = ?1", [t])?,
            None => self.conn.execute("UPDATE notifications SET read = 1", [])?,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_state_round_trip() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.timer_state("p", "t").unwrap(), None);
        db.put_timer_state("p", "t", r#"{"runs":1}"#).unwrap();
        db.put_timer_state("p", "t", r#"{"runs":2}"#).unwrap();
        assert_eq!(db.timer_state("p", "t").unwrap().as_deref(), Some(r#"{"runs":2}"#));
    }

    fn ws(id: &str, pos: i64) -> WorkspaceRow {
        WorkspaceRow {
            id: id.into(),
            name: id.into(),
            host: "local".into(),
            repo_path: "/r".into(),
            github_slug: Some("o/r".into()),
            default_branch: Some("main".into()),
            position: pos,
            pinned: false,
        }
    }

    #[test]
    fn workspaces_and_tabs() {
        let db = Db::open_in_memory().unwrap();
        db.insert_workspace(&ws("b", 2)).unwrap();
        db.insert_workspace(&ws("a", 1)).unwrap();
        assert_eq!(db.workspaces().unwrap().iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        let tab = TabRow {
            id: "t1".into(),
            workspace_id: "a".into(),
            title: "x".into(),
            worktree: "/r-x".into(),
            branch: Some("william/x".into()),
            tmux_session: "kt_a_t1".into(),
            position: 1,
            diff_base: Some("origin/main".into()),
        };
        db.insert_tab(&tab).unwrap();
        assert_eq!(db.tabs("a").unwrap(), vec![tab.clone()]);
        db.delete_workspace("a").unwrap();
        assert!(db.tab("t1").unwrap().is_none(), "tabs cascade with their workspace");
    }

    #[test]
    fn pinned_workspaces_sort_first_and_reorder_persists() {
        let db = Db::open_in_memory().unwrap();
        for (id, pos) in [("a", 0), ("b", 1), ("c", 2)] {
            db.insert_workspace(&ws(id, pos)).unwrap();
        }
        let order = |db: &Db| db.workspaces().unwrap().into_iter().map(|w| w.id).collect::<Vec<_>>();
        db.update_workspace(&WorkspaceRow { pinned: true, ..ws("c", 2) }).unwrap();
        assert_eq!(order(&db), ["c", "a", "b"]);
        db.reorder_workspaces(&[("b".into(), 0, false), ("a".into(), 1, false), ("c".into(), 2, false)]).unwrap();
        assert_eq!(order(&db), ["b", "a", "c"]);
    }

    #[test]
    fn migrate_adds_pinned_to_old_databases() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE workspaces (id TEXT PRIMARY KEY, name TEXT NOT NULL, host TEXT NOT NULL, repo_path TEXT NOT NULL,
             github_slug TEXT, default_branch TEXT, position INTEGER NOT NULL, created_at INTEGER NOT NULL);
             INSERT INTO workspaces VALUES ('a', 'a', 'local', '/r', NULL, NULL, 0, 0);",
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TABLE tabs (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, kind TEXT NOT NULL, title TEXT NOT NULL,
             worktree TEXT NOT NULL, branch TEXT, pr_number INTEGER, tmux_session TEXT NOT NULL UNIQUE, plan_file TEXT,
             position INTEGER NOT NULL, created_at INTEGER NOT NULL);
             INSERT INTO tabs VALUES ('t', 'a', 'readonly', 't', '/w', NULL, 12, 's', NULL, 1, 0);",
        )
        .unwrap();
        let db = Db::init(conn).unwrap();
        assert!(!db.workspaces().unwrap()[0].pinned);
        assert_eq!(db.tab("t").unwrap().unwrap().diff_base, None, "old tabs gain diff_base");
    }

    #[test]
    fn plan_comment_states() {
        let db = Db::open_in_memory().unwrap();
        db.insert_workspace(&ws("a", 1)).unwrap();
        db.insert_tab(&TabRow {
            id: "t".into(),
            workspace_id: "a".into(),
            title: "t".into(),
            worktree: "/w".into(),
            branch: None,
            tmux_session: "s".into(),
            position: 1,
            diff_base: None,
        })
        .unwrap();
        let id = db.add_plan_comment("t", 4, None, "## F1", "rename this").unwrap();
        assert!(db.set_plan_comment_state(id, "sent").unwrap());
        assert_eq!(db.plan_comments("t").unwrap()[0].state, "sent");
    }
}
