//! Books: the catalogue of titles and how many copies the library owns (spec §5.8).
//!
//! Layers: `routes` (HTTP) -> `service` (rules) -> `repo` (port) <- `sqlite` (adapter).
//! `domain` holds the validated types (`Isbn`, `BookFields`) the layers share.

mod domain;
mod repo;
mod routes;
mod service;
mod sqlite;

pub use domain::{BookFields, Isbn};
pub use repo::{BookFilter, BookRecord, BookRepository, DeleteBookOutcome, UpdateBookOutcome};
pub use routes::routes;
pub use service::BookService;
pub use sqlite::SqliteBookRepository;
