//! GitMesh core library.
//!
//! GitMesh presents one logical project to the user while that project is physically
//! stored in several Git repositories. Git itself remains the version-control engine:
//! every operation in this crate ultimately runs the `git` CLI. GitMesh never
//! reimplements Git internals.
//!
//! # Layering
//!
//! ```text
//!   cli / ui / gui    <- user interface, contains no Git logic
//!        |
//!   service           <- application layer: one API for every front end
//!        |
//!   ops               <- orchestration: one logical operation -> many physical repos
//!        |
//!   analyzer          <- unified status and change ownership
//!        |
//!   model, manifest, discovery   <- project configuration and repository model
//!        |
//!   git               <- safe Git CLI execution (never reimplements Git)
//! ```
//!
//! Each layer only depends on the layers below it. `manifest` and `discovery` write to
//! the filesystem (the manifest, `git init` when explicitly requested); everything
//! else is read-only except `ops`, which performs the actual Git operations.

pub mod analyzer;
pub mod cli;
pub mod discovery;
pub mod error;
pub mod git;
pub mod gui;
pub mod json;
pub mod manifest;
pub mod model;
pub mod ops;
pub mod paths;
pub mod providers;
pub mod service;
pub mod testkit;
pub mod ui;

pub use error::{Error, Result};

/// Version of the GitMesh crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
