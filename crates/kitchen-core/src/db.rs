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
    created_at     INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS tabs (
    id           TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('edit', 'readonly')),
    title        TEXT NOT NULL,
    worktree     TEXT NOT NULL,
    branch       TEXT,
    pr_number    INTEGER,
    tmux_session TEXT NOT NULL UNIQUE,
    plan_file    TEXT,
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

-- One automatic review of someone else's PR at one head commit.
CREATE TABLE IF NOT EXISTS review_runs (
    id           INTEGER PRIMARY KEY,
    github_slug  TEXT NOT NULL,
    pr_number    INTEGER NOT NULL,
    head_sha     TEXT NOT NULL,
    author       TEXT,
    title        TEXT,
    url          TEXT,
    workspace_id TEXT,
    worktree     TEXT,
    tmux_session TEXT,
    status       TEXT NOT NULL,            -- queued | running | done | failed | superseded
    error        TEXT,
    started_at   INTEGER,
    finished_at  INTEGER,
    UNIQUE (github_slug, pr_number, head_sha)
);

-- Agent-written review comments. They never reach GitHub until William
-- approves each one: state goes draft -> published (or rejected).
CREATE TABLE IF NOT EXISTS drafts (
    id                INTEGER PRIMARY KEY,
    run_id            INTEGER REFERENCES review_runs(id) ON DELETE SET NULL,
    github_slug       TEXT NOT NULL,
    pr_number         INTEGER NOT NULL,
    head_sha          TEXT,
    path              TEXT NOT NULL,
    line              INTEGER,             -- NULL = file-level comment
    start_line        INTEGER,
    side              TEXT NOT NULL DEFAULT 'RIGHT' CHECK (side IN ('LEFT', 'RIGHT')),
    body              TEXT NOT NULL,
    severity          TEXT,
    state             TEXT NOT NULL DEFAULT 'draft' CHECK (state IN ('draft', 'published', 'rejected')),
    github_comment_id INTEGER,
    github_url        TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS watch_list (
    login TEXT PRIMARY KEY
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TabRow {
    pub id: String,
    pub workspace_id: String,
    pub kind: String,
    pub title: String,
    pub worktree: String,
    pub branch: Option<String>,
    pub pr_number: Option<i64>,
    pub tmux_session: String,
    pub plan_file: Option<String>,
    pub position: i64,
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
pub struct ReviewRun {
    pub id: i64,
    pub github_slug: String,
    pub pr_number: i64,
    pub head_sha: String,
    pub author: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub workspace_id: Option<String>,
    pub worktree: Option<String>,
    pub tmux_session: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Draft {
    pub id: i64,
    pub run_id: Option<i64>,
    pub github_slug: String,
    pub pr_number: i64,
    pub head_sha: Option<String>,
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: String,
    pub body: String,
    pub severity: Option<String>,
    pub state: String,
    pub github_comment_id: Option<i64>,
    pub github_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct NewDraft {
    pub run_id: Option<i64>,
    pub github_slug: String,
    pub pr_number: i64,
    pub head_sha: Option<String>,
    pub path: String,
    pub line: Option<i64>,
    pub start_line: Option<i64>,
    pub side: Option<String>,
    pub body: String,
    pub severity: Option<String>,
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
    })
}

fn tab_from(r: &Row) -> rusqlite::Result<TabRow> {
    Ok(TabRow {
        id: r.get(0)?,
        workspace_id: r.get(1)?,
        kind: r.get(2)?,
        title: r.get(3)?,
        worktree: r.get(4)?,
        branch: r.get(5)?,
        pr_number: r.get(6)?,
        tmux_session: r.get(7)?,
        plan_file: r.get(8)?,
        position: r.get(9)?,
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

fn run_from(r: &Row) -> rusqlite::Result<ReviewRun> {
    Ok(ReviewRun {
        id: r.get(0)?,
        github_slug: r.get(1)?,
        pr_number: r.get(2)?,
        head_sha: r.get(3)?,
        author: r.get(4)?,
        title: r.get(5)?,
        url: r.get(6)?,
        workspace_id: r.get(7)?,
        worktree: r.get(8)?,
        tmux_session: r.get(9)?,
        status: r.get(10)?,
        error: r.get(11)?,
        started_at: r.get(12)?,
        finished_at: r.get(13)?,
    })
}

fn draft_from(r: &Row) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: r.get(0)?,
        run_id: r.get(1)?,
        github_slug: r.get(2)?,
        pr_number: r.get(3)?,
        head_sha: r.get(4)?,
        path: r.get(5)?,
        line: r.get(6)?,
        start_line: r.get(7)?,
        side: r.get(8)?,
        body: r.get(9)?,
        severity: r.get(10)?,
        state: r.get(11)?,
        github_comment_id: r.get(12)?,
        github_url: r.get(13)?,
    })
}

const WS_COLS: &str = "id, name, host, repo_path, github_slug, default_branch, position";
const TAB_COLS: &str = "id, workspace_id, kind, title, worktree, branch, pr_number, tmux_session, plan_file, position";
const PC_COLS: &str = "id, tab_id, line, end_line, quote, body, state, created_at";
const RUN_COLS: &str = "id, github_slug, pr_number, head_sha, author, title, url, workspace_id, worktree, tmux_session, status, error, started_at, finished_at";
const DRAFT_COLS: &str = "id, run_id, github_slug, pr_number, head_sha, path, line, start_line, side, body, severity, state, github_comment_id, github_url";

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
        Ok(Db { conn })
    }

    // ------------------------------------------------------------ workspaces

    pub fn workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let mut st = self.conn.prepare(&format!("SELECT {WS_COLS} FROM workspaces ORDER BY position, created_at"))?;
        Ok(st.query_map([], workspace_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn insert_workspace(&self, w: &WorkspaceRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO workspaces (id, name, host, repo_path, github_slug, default_branch, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![w.id, w.name, w.host, w.repo_path, w.github_slug, w.default_branch, w.position, now()],
        )?;
        Ok(())
    }

    pub fn update_workspace(&self, w: &WorkspaceRow) -> Result<()> {
        self.conn.execute(
            "UPDATE workspaces SET name = ?2, host = ?3, repo_path = ?4, github_slug = ?5, default_branch = ?6, position = ?7
             WHERE id = ?1",
            params![w.id, w.name, w.host, w.repo_path, w.github_slug, w.default_branch, w.position],
        )?;
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
            "INSERT INTO tabs (id, workspace_id, kind, title, worktree, branch, pr_number, tmux_session, plan_file, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                t.id,
                t.workspace_id,
                t.kind,
                t.title,
                t.worktree,
                t.branch,
                t.pr_number,
                t.tmux_session,
                t.plan_file,
                t.position,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn update_tab(&self, t: &TabRow) -> Result<()> {
        self.conn.execute(
            "UPDATE tabs SET title = ?2, worktree = ?3, branch = ?4, pr_number = ?5, plan_file = ?6, position = ?7 WHERE id = ?1",
            params![t.id, t.title, t.worktree, t.branch, t.pr_number, t.plan_file, t.position],
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

    pub fn delete_plan_comment(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM plan_comments WHERE id = ?1", [id])?;
        Ok(())
    }

    // ------------------------------------------------------------ review runs

    /// Insert a run unless one already exists for this exact head commit.
    /// Returns the new id, or `None` when the PR@sha was already reviewed.
    pub fn insert_review_run(
        &self,
        slug: &str,
        pr: i64,
        head_sha: &str,
        author: Option<&str>,
        title: Option<&str>,
        url: Option<&str>,
    ) -> Result<Option<i64>> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO review_runs (github_slug, pr_number, head_sha, author, title, url, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued')",
            params![slug, pr, head_sha, author, title, url],
        )?;
        if n == 0 {
            return Ok(None);
        }
        let id = self.conn.last_insert_rowid();
        // Older runs of the same PR are superseded by the new head.
        self.conn.execute(
            "UPDATE review_runs SET status = 'superseded'
             WHERE github_slug = ?1 AND pr_number = ?2 AND id != ?3 AND status IN ('queued', 'running')",
            params![slug, pr, id],
        )?;
        Ok(Some(id))
    }

    pub fn review_runs(&self, limit: i64) -> Result<Vec<ReviewRun>> {
        let mut st = self.conn.prepare(&format!("SELECT {RUN_COLS} FROM review_runs ORDER BY id DESC LIMIT ?1"))?;
        Ok(st.query_map([limit], run_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn review_run(&self, id: i64) -> Result<Option<ReviewRun>> {
        Ok(self.conn.query_row(&format!("SELECT {RUN_COLS} FROM review_runs WHERE id = ?1"), [id], run_from).optional()?)
    }

    pub fn runs_with_status(&self, status: &str) -> Result<Vec<ReviewRun>> {
        let mut st = self.conn.prepare(&format!("SELECT {RUN_COLS} FROM review_runs WHERE status = ?1 ORDER BY id"))?;
        Ok(st.query_map([status], run_from)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Re-queue a failed (or crash-orphaned queued) run of this exact head (manual retry).
    pub fn requeue_failed_run(&self, slug: &str, pr: i64, head_sha: &str) -> Result<Option<i64>> {
        let id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM review_runs WHERE github_slug = ?1 AND pr_number = ?2 AND head_sha = ?3 AND status IN ('failed', 'queued')",
                params![slug, pr, head_sha],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            self.conn.execute(
                "UPDATE review_runs SET status = 'queued', error = NULL, started_at = NULL, finished_at = NULL WHERE id = ?1",
                [id],
            )?;
        }
        Ok(id)
    }

    pub fn start_review_run(&self, id: i64, workspace_id: &str, worktree: &str, session: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE review_runs SET status = 'running', workspace_id = ?2, worktree = ?3, tmux_session = ?4, started_at = ?5
             WHERE id = ?1",
            params![id, workspace_id, worktree, session, now()],
        )?;
        Ok(())
    }

    pub fn finish_review_run(&self, id: i64, status: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE review_runs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
            params![id, status, error, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ drafts

    pub fn add_draft(&self, d: &NewDraft) -> Result<i64> {
        let side = d.side.clone().unwrap_or_else(|| "RIGHT".into()).to_uppercase();
        self.conn.execute(
            "INSERT INTO drafts (run_id, github_slug, pr_number, head_sha, path, line, start_line, side, body, severity, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
            params![
                d.run_id,
                d.github_slug,
                d.pr_number,
                d.head_sha,
                d.path,
                d.line,
                d.start_line,
                side,
                d.body,
                d.severity,
                now()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn drafts_for_pr(&self, slug: &str, pr: i64) -> Result<Vec<Draft>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {DRAFT_COLS} FROM drafts WHERE github_slug = ?1 AND pr_number = ?2 ORDER BY path, line, id"
        ))?;
        Ok(st.query_map(params![slug, pr], draft_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn drafts_with_state(&self, state: &str) -> Result<Vec<Draft>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {DRAFT_COLS} FROM drafts WHERE state = ?1 ORDER BY github_slug, pr_number, path, line, id"))?;
        Ok(st.query_map([state], draft_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn drafts_for_run(&self, run_id: i64) -> Result<Vec<Draft>> {
        let mut st = self.conn.prepare(&format!("SELECT {DRAFT_COLS} FROM drafts WHERE run_id = ?1 ORDER BY path, line, id"))?;
        Ok(st.query_map([run_id], draft_from)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn count_drafts(&self, state: &str) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COUNT(*) FROM drafts WHERE state = ?1", [state], |r| r.get(0))?)
    }

    pub fn draft(&self, id: i64) -> Result<Option<Draft>> {
        Ok(self.conn.query_row(&format!("SELECT {DRAFT_COLS} FROM drafts WHERE id = ?1"), [id], draft_from).optional()?)
    }

    pub fn edit_draft(&self, id: i64, body: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE drafts SET body = ?2, updated_at = ?3 WHERE id = ?1 AND state = 'draft'",
            params![id, body, now()],
        )?;
        Ok(())
    }

    pub fn reject_draft(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE drafts SET state = 'rejected', updated_at = ?2 WHERE id = ?1 AND state = 'draft'",
            params![id, now()],
        )?;
        Ok(())
    }

    /// The only transition to `published`; called after GitHub accepted it.
    pub fn mark_draft_published(&self, id: i64, comment_id: Option<i64>, url: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE drafts SET state = 'published', github_comment_id = ?2, github_url = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, comment_id, url, now()],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ watch list

    pub fn watch_list(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT login FROM watch_list ORDER BY login")?;
        Ok(st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn watch(&self, login: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO watch_list (login) VALUES (?1)", [login.trim_start_matches('@')])?;
        Ok(())
    }

    pub fn unwatch(&self, login: &str) -> Result<()> {
        self.conn.execute("DELETE FROM watch_list WHERE login = ?1", [login.trim_start_matches('@')])?;
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

    fn ws(id: &str, pos: i64) -> WorkspaceRow {
        WorkspaceRow {
            id: id.into(),
            name: id.into(),
            host: "local".into(),
            repo_path: "/r".into(),
            github_slug: Some("o/r".into()),
            default_branch: Some("main".into()),
            position: pos,
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
            kind: "edit".into(),
            title: "x".into(),
            worktree: "/r-x".into(),
            branch: Some("william/x".into()),
            pr_number: None,
            tmux_session: "kt_a_t1".into(),
            plan_file: None,
            position: 1,
        };
        db.insert_tab(&tab).unwrap();
        assert_eq!(db.tabs("a").unwrap(), vec![tab.clone()]);
        db.delete_workspace("a").unwrap();
        assert!(db.tab("t1").unwrap().is_none(), "tabs cascade with their workspace");
    }

    #[test]
    fn review_runs_dedup_and_supersede() {
        let db = Db::open_in_memory().unwrap();
        let r1 = db.insert_review_run("o/r", 5, "sha1", Some("bob"), None, None).unwrap();
        assert!(r1.is_some());
        assert!(db.insert_review_run("o/r", 5, "sha1", None, None, None).unwrap().is_none());
        let r2 = db.insert_review_run("o/r", 5, "sha2", None, None, None).unwrap().unwrap();
        assert_eq!(db.review_run(r1.unwrap()).unwrap().unwrap().status, "superseded");
        assert_eq!(db.review_run(r2).unwrap().unwrap().status, "queued");
    }

    #[test]
    fn requeue_only_failed_or_orphaned() {
        let db = Db::open_in_memory().unwrap();
        let id = db.insert_review_run("o/r", 1, "s", None, None, None).unwrap().unwrap();
        db.start_review_run(id, "w", "/wt", "sess").unwrap();
        assert_eq!(db.requeue_failed_run("o/r", 1, "s").unwrap(), None, "running runs are not retried");
        db.finish_review_run(id, "failed", Some("boom")).unwrap();
        assert_eq!(db.requeue_failed_run("o/r", 1, "s").unwrap(), Some(id));
        let run = db.review_run(id).unwrap().unwrap();
        assert_eq!((run.status.as_str(), run.error), ("queued", None));
        db.add_draft(&NewDraft { run_id: Some(id), github_slug: "o/r".into(), pr_number: 1, path: "a".into(), body: "b".into(), ..Default::default() }).unwrap();
        assert_eq!(db.drafts_for_run(id).unwrap().len(), 1);
        assert_eq!(db.count_drafts("draft").unwrap(), 1);
    }

    #[test]
    fn drafts_gate() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .add_draft(&NewDraft {
                github_slug: "o/r".into(),
                pr_number: 5,
                path: "a.rs".into(),
                line: Some(3),
                body: "nit".into(),
                ..Default::default()
            })
            .unwrap();
        db.edit_draft(id, "better nit").unwrap();
        let d = db.draft(id).unwrap().unwrap();
        assert_eq!((d.state.as_str(), d.body.as_str(), d.side.as_str()), ("draft", "better nit", "RIGHT"));
        db.mark_draft_published(id, Some(99), Some("u")).unwrap();
        db.edit_draft(id, "too late").unwrap();
        assert_eq!(db.draft(id).unwrap().unwrap().body, "better nit", "published drafts are frozen");
    }

    #[test]
    fn plan_comment_states() {
        let db = Db::open_in_memory().unwrap();
        db.insert_workspace(&ws("a", 1)).unwrap();
        db.insert_tab(&TabRow {
            id: "t".into(),
            workspace_id: "a".into(),
            kind: "edit".into(),
            title: "t".into(),
            worktree: "/w".into(),
            branch: None,
            pr_number: None,
            tmux_session: "s".into(),
            plan_file: None,
            position: 1,
        })
        .unwrap();
        let id = db.add_plan_comment("t", 4, None, "## F1", "rename this").unwrap();
        assert!(db.set_plan_comment_state(id, "sent").unwrap());
        assert_eq!(db.plan_comments("t").unwrap()[0].state, "sent");
    }
}
