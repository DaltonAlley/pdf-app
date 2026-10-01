//! HTTP backend for the self-hostable PDF Tools application.
//!
//! The crate exposes the application router and its runtime state so the
//! production binary and black-box integration tests use the same setup.
#![deny(missing_docs)]
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

mod adapters;
mod documents;
mod error;
mod imposition;
mod jobs;
mod progress;
mod web;

pub use error::{AppError, AppResult};
pub use web::{
    app, app_with_frontend_dist, bounded_setting, megabytes_to_bytes, read_env_u16, read_env_usize,
    read_optional_env_usize, AppState, FrontendDist, MAX_CONFIGURED_MEGABYTES,
};

pub(crate) const MAX_SOURCE_PDF_PAGES: usize = 1_000;

#[cfg(test)]
pub(crate) use adapters::test_pdfium;
