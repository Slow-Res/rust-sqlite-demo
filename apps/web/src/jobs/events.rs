//! Loan events: what borrowing or returning a book tells the outside world
//! (spec §15.1).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoanEventKind {
    Borrowed,
    Returned,
}

impl LoanEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Borrowed => "borrowed",
            Self::Returned => "returned",
        }
    }
}

/// `loan.<id>.<borrowed|returned>`: the same for every job and attempt of one
/// event, so receivers can de-duplicate on it.
pub fn event_id(loan_id: i64, kind: LoanEventKind) -> String {
    format!("loan.{loan_id}.{}", kind.as_str())
}
