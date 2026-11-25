//! HTTP API server.
//!
//! Implements the REST API for external control and monitoring of the
//! miner. Built on Axum, binds to localhost only by default and does not
//! require authentication for local access.

pub mod commands;
#[cfg(all(feature = "nano3s", target_os = "linux"))]
mod bluetooth;
mod dashboard;
mod registry;
mod server;
mod v0;

pub use registry::BoardRegistration;
pub use server::{ApiConfig, serve};
