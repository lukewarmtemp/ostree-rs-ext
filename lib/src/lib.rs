//! # Extension APIs for ostree
//!
//! This crate builds on top of the core ostree C library
//! and the Rust bindings to it, adding new functionality
//! written in Rust.  

// See https://doc.rust-lang.org/rustc/lints/listing/allowed-by-default.html
#![deny(missing_docs)]
#![deny(missing_debug_implementations)]
#![forbid(unused_must_use)]
#![deny(unsafe_code)]
#![cfg_attr(feature = "dox", feature(doc_cfg))]
#![deny(clippy::dbg_macro)]
#![deny(clippy::todo)]

// Re-export our dependencies.  See https://gtk-rs.org/blog/2021/06/22/new-release.html
// "Dependencies are re-exported".  Users will need e.g. `gio::File`, so this avoids
// them needing to update matching versions.
pub use containers_image_proxy::oci_spec;
pub use ostree;
pub use ostree::gio;
pub use ostree::gio::glib;

/// Our generic catchall fatal error, expected to be converted
/// to a string to output to a terminal or logs.
type Result<T> = anyhow::Result<T>;

// Import global functions.
mod globals;

mod isolation;

pub mod bootabletree;
pub mod cli;
pub mod container;
pub mod container_utils;
pub mod diff;
pub mod ima;
pub mod keyfileext;
pub(crate) mod logging;
pub mod mountutil;
pub mod refescape;
#[doc(hidden)]
pub mod repair;
pub mod sysroot;
pub mod tar;
pub mod tokio_util;

pub mod selinux;

pub mod chunking;
pub mod commit;
pub mod objectsource;
pub(crate) mod objgv;
#[cfg(feature = "internal-testing-api")]
pub mod ostree_manual;
pub(crate) mod statistics;

mod utils;

#[cfg(feature = "docgen")]
mod docgen;

/// Prelude, intended for glob import.
pub mod prelude {
    #[doc(hidden)]
    pub use ostree::prelude::*;
}

#[cfg(feature = "internal-testing-api")]
pub mod fixture;
#[cfg(feature = "internal-testing-api")]
pub mod integrationtest;
#[cfg(feature = "internal-testing-api")]
pub use container::ocidir;
