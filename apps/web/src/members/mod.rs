//! Members: the people who borrow books (spec §5.8). Every endpoint requires a librarian.
//!
//! Same layering as `books`: `routes` -> `service` -> `repo` (port) <- `sqlite` (adapter),
//! with `domain` holding the validated types (`Email`, `MemberFields`).

mod domain;
mod repo;
mod routes;
mod service;
mod sqlite;

pub use domain::{Email, MemberFields};
pub use repo::{DeleteMemberOutcome, MemberFilter, MemberRecord, MemberRepository};
pub use routes::routes;
pub use service::MemberService;
pub use sqlite::SqliteMemberRepository;
