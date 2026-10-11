//! Member DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MemberQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub q: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct MemberInput {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Member {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: i64,
    pub name: String,
    pub email: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub active_loans: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_round_trips() {
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let member = Member {
            id: 2,
            name: "Grace".into(),
            email: "grace@example.com".into(),
            active_loans: 1,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&member).unwrap();
        assert_eq!(serde_json::from_str::<Member>(&json).unwrap(), member);
    }
}
