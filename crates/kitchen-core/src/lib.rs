//! Host-agnostic core of the kitchen: everything that is not GUI.
//!
//! Every operation that touches a repository goes through [`host::Host`], so
//! a workspace on a devbox and a workspace on the mac run the same code; only
//! the transport (a local process vs. `ssh <alias> ...`) differs.

pub mod bootstrap;
pub mod db;
pub mod git;
pub mod host;
pub mod plan;
pub mod summaries;
pub mod tmux;
pub mod util;
