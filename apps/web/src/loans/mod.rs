//! Loans: who borrowed which book and when it is due (spec §2, §5.6, §5.8).
//! Every endpoint requires a librarian.
//!
//! Same layering as `books` and `members`: `routes` -> `service` -> `repo` (port)
//! <- `sqlite` (adapter). The loan rules live in `service` (policy: loan period,
//! overdue) and in `sqlite` (checks that must be atomic: copies, member limit).
//! "Borrow a book" is `routes::create` -> `LoanService::create` ->
//! `LoanRepository::create_checked`.

mod repo;
mod routes;
mod service;
mod sqlite;

pub use repo::{
    CreateLoanOutcome, LoanDetails, LoanFilter, LoanRepository, NewLoanRecord, ReturnLoanOutcome,
};
pub use routes::routes;
pub use service::LoanService;
pub use sqlite::SqliteLoanRepository;

/// How long a member may keep a book.
pub const LOAN_PERIOD_DAYS: i64 = 14;
/// How many unreturned loans one member may hold at once.
pub const MAX_ACTIVE_LOANS_PER_MEMBER: i64 = 5;
