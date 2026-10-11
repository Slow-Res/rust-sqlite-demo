//! Loan DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Status filter for the loan list. `Active` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum LoanStatus {
    Active,
    Overdue,
    Returned,
    All,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LoanQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub status: Option<LoanStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub member_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional, type = "number"))]
    pub book_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct NewLoan {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub book_id: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub member_id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Loan {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub book_id: i64,
    pub book_title: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub member_id: i64,
    pub member_name: String,
    pub borrowed_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub returned_at: Option<DateTime<Utc>>,
    pub is_overdue: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&LoanStatus::Overdue).unwrap(),
            "\"overdue\""
        );
    }

    #[test]
    fn loan_round_trips() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let loan = Loan {
            id: 1,
            book_id: 2,
            book_title: "Dune".into(),
            member_id: 3,
            member_name: "Grace".into(),
            borrowed_at: now,
            due_at: now,
            returned_at: None,
            is_overdue: false,
        };
        let json = serde_json::to_string(&loan).unwrap();
        assert_eq!(serde_json::from_str::<Loan>(&json).unwrap(), loan);
    }
}
