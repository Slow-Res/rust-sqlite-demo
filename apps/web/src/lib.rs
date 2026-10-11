//! Library management backend: a JSON API over SQLite.
//!
//! The binary (`main.rs`) is a thin shell; everything testable lives in this
//! library so integration tests can drive the real router.

pub mod app;
pub mod auth;
pub mod books;
pub mod cli;
pub mod clock;
pub mod commands;
pub mod config;
pub mod db;
pub mod error;
pub mod extract;
pub mod health;
pub mod loans;
pub mod members;
pub mod pagination;
pub mod seed;
pub mod server;
pub mod state;
pub mod telemetry;

pub use app::build_router;
