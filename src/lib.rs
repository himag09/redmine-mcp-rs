//! Library surface for the Redmine MCP server.
//!
//! The binary is a thin wrapper around these modules; integration tests use
//! them directly.

pub mod config;
pub mod error;
pub mod log;
pub mod redmine;
pub mod server;
pub mod tools;
