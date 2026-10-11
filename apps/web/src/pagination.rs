//! Normalization of list query parameters: `page` / `per_page` and the search
//! term `q` (spec §5.8).

use library_shared::Page;

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

/// Validated pagination: `page >= 1`, `per_page` within `1..=100`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageParams {
    page: u32,
    per_page: u32,
}

impl PageParams {
    /// Apply defaults and clamp out-of-range values instead of rejecting them.
    pub fn new(page: Option<u32>, per_page: Option<u32>) -> Self {
        Self {
            page: page.unwrap_or(1).max(1),
            per_page: per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
        }
    }

    pub fn page(self) -> u32 {
        self.page
    }

    pub fn per_page(self) -> u32 {
        self.per_page
    }

    /// SQL `LIMIT`. `i64` because that is SQLite's integer type.
    pub fn limit(self) -> i64 {
        i64::from(self.per_page)
    }

    /// SQL `OFFSET`. Computed in `i64`, so it cannot overflow for any `u32` inputs.
    pub fn offset(self) -> i64 {
        i64::from(self.page - 1) * i64::from(self.per_page)
    }

    /// Wrap a fetched slice of rows into the response envelope.
    pub fn into_page<T>(self, items: Vec<T>, total: u64) -> Page<T> {
        Page {
            items,
            page: self.page,
            per_page: self.per_page,
            total,
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
        assert_eq!((p.limit(), p.offset()), (25, 50));
        assert_eq!(
            PageParams::new(Some(u32::MAX), Some(100)).offset(),
            i64::from(u32::MAX - 1) * 100
        );
    }

    #[test]
    fn builds_page_envelope() {
        let page = PageParams::new(Some(2), Some(10)).into_page(vec![1, 2], 12);
        assert_eq!((page.page, page.per_page, page.total), (2, 10, 12));
    }
}
