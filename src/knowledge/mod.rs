//! Authoritative OKF knowledge primitives. No database connection is required.
//! Command adapters are layered on these primitives; OKF is the only knowledge backend.
pub mod clients;
pub mod document;
pub mod guard;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod identity;
pub mod legacy;
pub mod matching;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod service;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod store;
pub mod telemetry;
pub(crate) mod timing;
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
pub mod relocation;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod bootstrap;
