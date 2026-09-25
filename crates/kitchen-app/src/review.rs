//! GitHub PR review: human comments from the viewer, agent drafts, the
//! approval gate, resolution sync, and auto-review of watched people's PRs.
//!
//! INTERFACE CONTRACT (used by the viewer; implemented by the review work):
//! all functions are cheap for the UI thread unless marked "blocking".

use std::sync::Arc;

use anyhow::Result;
use kitchen_core::db::Draft;
use kitchen_core::gh::{CommentAnchor, PostedComment, PrInfo, ReviewThread};
use kitchen_proto::Caller;
use serde_json::Value;

use crate::engine::Engine;

/// The PR a tab works on (its `pr_number`, or the open PR for its branch).
/// Cached; returns `None` until a background lookup finishes.
pub fn pr_for_tab(_engine: &Arc<Engine>, _tab_id: &str) -> Option<PrInfo> {
    None
}

/// Review threads of the tab's PR (cached, refreshed in the background
/// every ~60s while requested).
pub fn threads_for_tab(_engine: &Arc<Engine>, _tab_id: &str) -> Vec<ReviewThread> {
    Vec::new()
}

/// Agent drafts (not yet public) for the tab's PR, shown inline in the viewer.
pub fn drafts_for_tab(_engine: &Arc<Engine>, _tab_id: &str) -> Vec<Draft> {
    Vec::new()
}

/// Blocking. Publish William's own comment (typed with ⌘M, sent with ⌘Enter)
/// to the tab's PR. Falls back to a file-level comment when the line is not
/// in the PR diff.
pub fn post_comment(_engine: &Arc<Engine>, _tab_id: &str, _anchor: CommentAnchor, _body: &str) -> Result<PostedComment> {
    anyhow::bail!("not implemented")
}

/// Blocking, UI-only (never reachable from the socket API): publish one
/// approved agent draft.
pub fn approve_draft(_engine: &Arc<Engine>, _draft_id: i64) -> Result<PostedComment> {
    anyhow::bail!("not implemented")
}

pub fn reject_draft(_engine: &Arc<Engine>, _draft_id: i64) -> Result<()> {
    anyhow::bail!("not implemented")
}

/// API methods under `review.*` and `pr.*`. `None` = not ours.
pub fn dispatch(_engine: &Arc<Engine>, _method: &str, _params: Value, _caller: &Caller) -> Option<Result<Value>> {
    None
}
