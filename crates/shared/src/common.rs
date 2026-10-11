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
}

#[cfg(test)]
mod tests {
    use super::*;

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
