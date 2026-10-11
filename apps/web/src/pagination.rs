//! Normalization of list query parameters: `page` / `per_page` and the search
//! term `q` (spec §5.8).

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use library_shared::Page;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{AppError, FieldErrors};

pub const DEFAULT_PER_PAGE: u32 = 20;
pub const MAX_PER_PAGE: u32 = 100;
/// SQLite rejects very long `LIKE` patterns (as a 500), so cap the term up front.
const MAX_SEARCH_CHARS: usize = 200;

/// Trim a search term; blank means "no filter". Longer than
/// [`MAX_SEARCH_CHARS`] characters is a 422 on field `q`.
pub fn search_term(q: Option<String>) -> Result<Option<String>, AppError> {
    let Some(q) = q.map(|q| q.trim().to_owned()).filter(|q| !q.is_empty()) else {
        return Ok(None);
    };
    if q.chars().count() > MAX_SEARCH_CHARS {
        let mut errors = FieldErrors::new();
        errors.add("q", "must be at most 200 characters");
        return Err(AppError::Validation(errors));
    }
    Ok(Some(q))
}

/// Encode a keyset position (the sort key and the id of the last item) as an
/// opaque URL-safe string: base64 of the JSON `[key, id]`.
pub fn encode_cursor<K: Serialize>(key: &K, id: i64) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::json!([key, id]).to_string())
}

/// Decode a cursor into typed values, which the adapters bind as parameters
/// (never into SQL text). Anything that is not what [`encode_cursor`] made for
/// this key type is a 400.
pub fn decode_cursor<K: DeserializeOwned>(cursor: &str) -> Result<(K, i64), AppError> {
    let invalid = || AppError::BadRequest("invalid cursor".to_owned());
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

/// Validated pagination: `page >= 1`, `per_page` within `1..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    page: u32,
    per_page: u32,
    /// An `after` cursor was given: `page` is ignored, so the offset is 0.
    keyset: bool,
}

impl PageParams {
    /// Apply defaults and clamp out-of-range values instead of rejecting them.
    pub fn new(page: Option<u32>, per_page: Option<u32>) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            per_page: per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
            keyset: false,
        }
    }

    /// `new`, plus the decoded `after` cursor if one was given. With a cursor
    /// `page` is ignored (see [`keyset`](Self::keyset)).
    pub fn with_after<K: DeserializeOwned>(
        page: Option<u32>,
        per_page: Option<u32>,
        after: Option<&str>,
    ) -> Result<(Self, Option<(K, i64)>), AppError> {
        let params = Self::new(page, per_page);
        match after.map(decode_cursor::<K>).transpose()? {
            Some(after) => Ok((params.keyset(), Some(after))),
            None => Ok((params, None)),
        }
    }

    /// Mark the request as a keyset one (`after` given): the offset becomes 0.
    pub fn keyset(self) -> Self {
        Self {
            keyset: true,
            ..self
        }
    }

    pub fn page(self) -> u32 {
        self.page
    }

    pub fn per_page(self) -> u32 {
        self.per_page
    }

    /// SQL `LIMIT`: one row more than the page, to learn whether a next page
    /// exists. `i64` because that is SQLite's integer type.
    pub fn limit(self) -> i64 {
        i64::from(self.per_page) + 1
    }

    /// Cut the `per_page + 1` fetched rows down to the page; the flag says
    /// whether a row was left over, i.e. there is a next page.
    pub fn split<T>(self, mut rows: Vec<T>) -> (Vec<T>, bool) {
        let more = rows.len() > self.per_page as usize;
        rows.truncate(self.per_page as usize);
        (rows, more)
    }

    /// SQL `OFFSET`. Computed in `i64`, so it cannot overflow for any `u32` inputs.
    /// Always 0 for a keyset request.
    pub fn offset(self) -> i64 {
        if self.keyset {
            return 0;
        }
        i64::from(self.page - 1) * i64::from(self.per_page)
    }

    /// Wrap a fetched slice of rows into the response envelope. `next_cursor`
    /// is the cursor of the last item, if [`split`](Self::split) found more.
    pub fn into_page<T>(self, items: Vec<T>, total: u64, next_cursor: Option<String>) -> Page<T> {
        Page {
            items,
            page: self.page,
            per_page: self.per_page,
            total,
            next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let p = PageParams::new(None, None);
        assert_eq!((p.page(), p.per_page(), p.offset()), (1, 20, 0));
    }

    #[test]
    fn clamps_out_of_range_values() {
        let p = PageParams::new(Some(0), Some(0));
        assert_eq!((p.page(), p.per_page()), (1, 1));
        assert_eq!(PageParams::new(None, Some(500)).per_page(), 100);
    }

    #[test]
    fn offset_math() {
        let p = PageParams::new(Some(3), Some(25));
        assert_eq!((p.limit(), p.offset()), (26, 50));
        assert_eq!(
            PageParams::new(Some(u32::MAX), Some(100)).offset(),
            i64::from(u32::MAX - 1) * 100
        );
    }

    #[test]
    fn builds_page_envelope() {
        let page = PageParams::new(Some(2), Some(10)).into_page(vec![1, 2], 12, None);
        assert_eq!((page.page, page.per_page, page.total), (2, 10, 12));
    }

    #[test]
    fn split_keeps_per_page_rows_and_reports_a_leftover() {
        let p = PageParams::new(None, Some(2));
        assert_eq!(p.split(vec![1, 2, 3]), (vec![1, 2], true));
        assert_eq!(p.split(vec![1, 2]), (vec![1, 2], false));
    }

    #[test]
    fn keyset_ignores_page() {
        let p = PageParams::new(Some(5), Some(10)).keyset();
        assert_eq!(p.offset(), 0);
    }

    #[test]
    fn cursor_round_trips() {
        let cursor = encode_cursor(&"Dune".to_owned(), 7);
        assert_eq!(
            decode_cursor::<String>(&cursor).unwrap(),
            ("Dune".to_owned(), 7)
        );
        let cursor = encode_cursor(&1_700_000_000_i64, 9);
        assert_eq!(decode_cursor::<i64>(&cursor).unwrap(), (1_700_000_000, 9));
    }

    #[test]
    fn malformed_cursors_are_bad_requests() {
        for bad in ["", "!!!", "e30", "WyJhIl0", "WyJhIiwiYiJd"] {
            assert!(
                matches!(decode_cursor::<String>(bad), Err(AppError::BadRequest(_))),
                "{bad:?}"
            );
        }
        // Valid cursor, wrong key type.
        let cursor = encode_cursor(&"x".to_owned(), 1);
        assert!(decode_cursor::<i64>(&cursor).is_err());
    }
}
