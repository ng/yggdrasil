//! Authoritative OKF knowledge primitives. No database connection is required.
//! Command adapters and guarded legacy cutover are layered on these primitives.
pub mod document;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod identity;
pub mod matching;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod service;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod store;
mod yaml;
