//! Authentication DTOs.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// The logged-in librarian.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CurrentUser {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub id: i64,
    pub username: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_round_trips() {
        let user = CurrentUser {
            id: 7,
            username: "ada".into(),
        };
        let json = serde_json::to_string(&user).unwrap();
        assert_eq!(serde_json::from_str::<CurrentUser>(&json).unwrap(), user);
    }
}
