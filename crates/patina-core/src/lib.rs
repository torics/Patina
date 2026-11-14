//! Domain types and configuration shared by every Patina crate.
//! This crate must not depend on axum, SQLite or any provider SDK.

pub mod agent;
pub mod agent_config;
pub mod api;
pub mod config;
pub mod events;
pub mod paths;
pub mod provider;
pub mod provider_config;
pub mod tool;

pub use config::{Config, ConfigError};
pub use paths::Paths;
