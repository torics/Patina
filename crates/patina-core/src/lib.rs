//! Domain types and configuration shared by every Patina crate.
//! This crate must not depend on axum, SQLite or any provider SDK.

pub mod config;
pub mod paths;

pub use config::{Config, ConfigError};
pub use paths::Paths;
