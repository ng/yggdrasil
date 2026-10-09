//! Authoritative OKF knowledge primitives. No database connection is required.
//! Command adapters and guarded legacy cutover are layered on these primitives.
pub mod clients;
pub mod document;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod export;
pub mod guard;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod identity;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod inventory;
pub mod legacy;
pub mod matching;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod service;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod store;
pub mod telemetry;
mod yaml;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod runtime;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod injection;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod usage;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod shared;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod backend;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod reverse;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod rollback;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod fence;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod forward;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod cutover;
