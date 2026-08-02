//! Library root for the accessible legal-service router.
//!
//! The crate is split into pure, testable pieces:
//! - [`model`]   — immutable, normalized catalog types + JSON parsing.
//! - [`routing`] — the pure two-phase routing function.
//! - [`store`]   — SQLite persistence + the versioned in-memory cache.
//! - [`api`]     — the Axum HTTP surface.

pub mod api;
pub mod model;
pub mod routing;
pub mod store;
