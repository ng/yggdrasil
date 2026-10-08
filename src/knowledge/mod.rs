//! Authoritative OKF knowledge primitives. No database connection is required.
//! Command adapters and guarded legacy cutover are layered on these primitives.
pub mod document;
mod yaml;
