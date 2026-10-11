//! API data types shared by the backend and the frontend.
//!
//! Plain data only: no IO, no framework types. With the `ts` feature enabled,
//! `cargo test -p library-shared --features ts` also writes TypeScript
//! bindings into `apps/frontend/src/generated/`.

pub mod auth;
pub mod books;
pub mod common;
pub mod loans;
pub mod members;

pub use auth::*;
pub use books::*;
pub use common::*;
pub use loans::*;
pub use members::*;
