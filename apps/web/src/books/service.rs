//! Book business rules: validation, search normalization, outcome-to-error mapping.
//! Knows nothing about HTTP or SQL.

use std::sync::Arc;

use chrono::Datelike;
use library_shared::{Availability, Book, BookInput, BookQuery, Page};

use super::domain::BookFields;
use super::repo::{BookFilter, BookRepository, DeleteBookOutcome, UpdateBookOutcome};
use crate::clock::Clock;
use crate::db::RepoError;
use crate::error::AppError;
use crate::pagination::{PageParams, search_term};

/// The unique index on `books.isbn` shows up as this constraint name.
const ISBN_CONSTRAINT: &str = "books.isbn";

#[derive(Clone)]
pub struct BookService {
    repo: Arc<dyn BookRepository>,
    clock: Arc<dyn Clock>,
}

impl BookService {
    pub fn new(repo: Arc<dyn BookRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { repo, clock }
    }

    pub async fn list(&self, query: BookQuery) -> Result<Page<Book>, AppError> {
        let page = PageParams::new(query.page, query.per_page);
        let filter = BookFilter {
            q: search_term(query.q)?,
            availability: query.availability.unwrap_or(Availability::All),
        };
        let (records, total) = self.repo.list(&filter, page).await?;
        Ok(page.into_page(records.into_iter().map(Book::from).collect(), total))
    }

    pub async fn get(&self, id: i64) -> Result<Book, AppError> {
        self.repo
            .get(id)
            .await?
            .map(Book::from)
            .ok_or(AppError::NotFound("book"))
    }

    pub async fn create(&self, input: BookInput) -> Result<Book, AppError> {
        let fields = self.parse(input)?;
        let record = self
            .repo
            .create(&fields, self.clock.now())
            .await
            .map_err(map_isbn_conflict)?;
        Ok(record.into())
    }

    pub async fn update(&self, id: i64, input: BookInput) -> Result<Book, AppError> {
        let fields = self.parse(input)?;
        match self
            .repo
            .update_checked(id, &fields, self.clock.now())
            .await
            .map_err(map_isbn_conflict)?
        {
            UpdateBookOutcome::Updated(record) => Ok(record.into()),
            UpdateBookOutcome::NotFound => Err(AppError::NotFound("book")),
            UpdateBookOutcome::CopiesBelowActiveLoans => Err(AppError::Conflict {
                code: "copies_below_active_loans",
                message: "total copies cannot be less than the copies currently on loan".to_owned(),
            }),
        }
    }

    pub async fn delete(&self, id: i64) -> Result<(), AppError> {
        match self.repo.delete_checked(id).await? {
            DeleteBookOutcome::Deleted => Ok(()),
            DeleteBookOutcome::NotFound => Err(AppError::NotFound("book")),
            DeleteBookOutcome::HasActiveLoans => Err(AppError::Conflict {
                code: "book_has_active_loans",
                message: "book has active loans and cannot be deleted".to_owned(),
            }),
        }
    }

    fn parse(&self, input: BookInput) -> Result<BookFields, AppError> {
        BookFields::parse(input, self.clock.now().year()).map_err(AppError::Validation)
    }
}

/// A duplicate ISBN is a client-visible 409; any other repo error stays internal.
fn map_isbn_conflict(err: RepoError) -> AppError {
    match err {
        RepoError::Conflict { constraint } if constraint == ISBN_CONSTRAINT => AppError::Conflict {
            code: "isbn_taken",
            message: "a book with this ISBN already exists".to_owned(),
        },
        other => other.into(),
    }
}
