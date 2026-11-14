//! Domain types and configuration shared by every Patina crate.
//! This crate must not depend on axum, SQLite or any provider SDK.

pub mod agent;
pub mod config;
pub mod events;
pub mod paths;
pub mod provider;
pub mod provider_config;

pub use config::{Config, ConfigError};
pub use paths::Paths;
