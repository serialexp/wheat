//! `.gitignore`-compatible ignore rules for Wheat workspaces.
//!
//! This crate provides a pattern engine that parses `.wheatignore` and
//! `.gitignore` files using Git's `gitignore(5)` syntax and evaluates paths
//! against a hierarchical stack of layered rules.
//!
//! # Architecture
//!
//! - [`pattern`] — Parses individual `.gitignore`-syntax lines into compiled
//!   glob matchers.
//! - [`stack`] — Manages a layered stack of ignore files with directory-scoped
//!   precedence, negation, and `.gitignore` fallback tracking.
//! - [`loader`] — File I/O for reading ignore files from disk and resolving
//!   XDG paths.
//!
//! # Usage
//!
//! ```no_run
//! use wheat_ignore::stack::IgnoreStack;
//! use wheat_ignore::loader::read_global_ignore;
//!
//! let global = read_global_ignore();
//! let global_ref = global.as_ref().map(|(c, p)| (c.as_str(), p.clone()));
//! let mut stack = IgnoreStack::new(global_ref, &[]);
//!
//! // Push directory rules during traversal.
//! // stack.push_directory(workspace_root);
//! // let result = stack.is_ignored("target", true);
//! ```

pub mod loader;
pub mod pattern;
pub mod stack;
