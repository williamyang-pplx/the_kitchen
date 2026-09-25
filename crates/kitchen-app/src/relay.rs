//! API relay for devbox workspaces. (stub)

use std::sync::Arc;

use crate::engine::Engine;

/// Keep one `ssh <alias> kitchenctl __relay` running for this host.
pub fn ensure_relay(_engine: &Arc<Engine>, _alias: &str) {}
