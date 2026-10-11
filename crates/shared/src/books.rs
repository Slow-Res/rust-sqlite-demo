//! Book DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Availability filter for the book list. `All` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    All,
    Available,
    OnLoan,
}

/// Query string of `GET /api/books`. Optional fields are skipped when `None`
/// so frontends can serialize this straight into a query string.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BookQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub availability: Option<Availability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_page: Option<u32>,
}

/// Body of `POST /api/books` and `PUT /api/books/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BookInput {
    pub title: String,
    pub author: String,
    pub isbn: Option<String>,
    pub published_year: Option<i32>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total_copies: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Book {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: i64,
    pub title: String,
    pub author: String,
    pub isbn: Option<String>,
    pub published_year: Option<i32>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total_copies: i64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub available_copies: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Availability::OnLoan).unwrap(),
            "\"on_loan\""
        );
    }

    #[test]
    fn book_round_trips() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let book = Book {
            id: 1,
            title: "Dune".into(),
            author: "Frank Herbert".into(),
            isbn: None,
            published_year: Some(1965),
            total_copies: 3,
            available_copies: 2,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&book).unwrap();
        assert_eq!(serde_json::from_str::<Book>(&json).unwrap(), book);
    }

    #[test]
    fn empty_query_serializes_to_empty_object() {
        assert_eq!(serde_json::to_string(&BookQuery::default()).unwrap(), "{}");
    }
}
