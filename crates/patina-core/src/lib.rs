//! Domain types and configuration shared by every Patina crate.
//! This crate must not depend on axum, SQLite or any provider SDK.

pub mod agent;
pub mod agent_config;
pub mod api;
pub mod budget;
pub mod compact;
pub mod config;
pub mod events;
pub mod paths;
pub mod policy;
pub mod project_config;
pub mod provider;
pub mod provider_config;
pub mod sandbox;
pub mod team;
pub mod team_config;
pub mod team_templates;
pub mod tool;

pub use config::{Config, ConfigError};
pub use paths::Paths;
