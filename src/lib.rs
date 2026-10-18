//! projj: review for "projj-style" roots, one git repository for human
//! review on top of jj repositories that agents work in.
//!
//! The `projj` command line and the desktop app (in `desktop/`) are both
//! built on this library: `api::router` is the whole app, and the two differ
//! only in how a page reaches it.

pub mod api;
pub mod bundled_jj;
pub mod chooser;
mod codex;
mod diff;
mod files;
pub mod finder;
pub mod gate;
mod git;
mod git_builtin;
mod guide;
pub mod jj;
pub mod root;
pub mod store;
mod syntax;

/// Which git answers: the one installed, or the one built in.
pub use git::Engine;
