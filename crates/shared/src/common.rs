//! Types used by every area of the API: pagination, errors, health.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One page of a list endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Page<T> {
    pub items: Vec<T>,
    pub page: u32,
    pub per_page: u32,
    /// Number of matching items over all pages. For an unfiltered list it may lag
    /// behind a write by up to `LIST_COUNT_TTL_MS` (default 1 s; 0 is exact).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: u64,
}

/// Error envelope returned for every non-2xx JSON response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    /// Per-field validation messages; omitted from the JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub fields: Option<BTreeMap<String, String>>,
}

/// Body of `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Health {
    pub status: String,
    /// Which node answered (spec §8): handy when checking sticky routing.
    pub node_id: String,
    pub version: String,
}

/// A node name: 1-40 characters of `a-z`, `0-9` and `-`, not starting with
/// `-`. It becomes the node's S3 folder and its Postgres `node_id`, so
/// anything else (slashes, spaces, capitals, newlines) is refused.
pub fn is_valid_node_id(id: &str) -> bool {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = id.chars();
    matches!(chars.next(), Some(first) if allowed(first))
        && id.len() <= 40
        && chars.all(|c| allowed(c) || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_that_are_accepted() {
        for id in ["local", "node1", "node-2", "a", "0branch", &"a".repeat(40)] {
            assert!(is_valid_node_id(id), "{id:?} should be valid");
        }
    }

    #[test]
    fn node_ids_that_are_refused() {
        for id in [
            "",
            "-node",
            "Node1",
            "node_1",
            "node 1",
            "a/b",
            "../x",
            "node1\nx",
            "nöde",
            &"a".repeat(41),
        ] {
            assert!(!is_valid_node_id(id), "{id:?} should be refused");
        }
    }

    #[test]
    fn page_round_trips() {
        let page = Page {
            items: vec![1, 2],
            page: 1,
            per_page: 20,
            total: 2,
        };
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<Page<i32>>(&json).unwrap(), page);
    }

    #[test]
    fn error_body_omits_fields_when_none() {
        let body = ApiErrorBody {
            code: "not_found".into(),
            message: "x".into(),
            fields: None,
        };
        let value = serde_json::to_value(&body).unwrap();
        assert!(value.get("fields").is_none());
    }

    #[test]
    fn error_body_keeps_fields_when_present() {
        let fields = BTreeMap::from([("title".to_string(), "must not be empty".to_string())]);
        let body = ApiErrorBody {
            code: "c".into(),
            message: "m".into(),
            fields: Some(fields),
        };
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["fields"]["title"], "must not be empty");
    }
}
