//! Sample data for a fresh database (`library-web seed`).
//!
//! Goes through the same repositories and validation (`BookFields::parse`,
//! `MemberFields::parse`) as the API, so seeded rows are indistinguishable
//! from ones a librarian would create.

use anyhow::{Context, bail};
use chrono::{DateTime, Datelike, Duration, Utc};
use library_shared::{Availability, BookInput, MemberInput};
use sqlx::SqlitePool;

use crate::books::{BookFields, BookFilter, BookRepository, SqliteBookRepository};
use crate::error::FieldErrors;
use crate::loans::{
    CreateLoanOutcome, LOAN_PERIOD_DAYS, LoanRepository, MAX_ACTIVE_LOANS_PER_MEMBER,
    NewLoanRecord, SqliteLoanRepository,
};
use crate::members::{MemberFields, MemberFilter, MemberRepository, SqliteMemberRepository};
use crate::pagination::PageParams;

/// `(title, author, isbn-13, year, copies)`
const BOOKS: [(&str, &str, &str, i32, i64); 12] = [
    ("Dune", "Frank Herbert", "9780441172719", 1965, 3),
    (
        "Nineteen Eighty-Four",
        "George Orwell",
        "9780451524935",
        1949,
        2,
    ),
    (
        "Pride and Prejudice",
        "Jane Austen",
        "9780141439518",
        1813,
        2,
    ),
    (
        "To Kill a Mockingbird",
        "Harper Lee",
        "9780061120084",
        1960,
        1,
    ),
    ("The Hobbit", "J. R. R. Tolkien", "9780547928227", 1937, 3),
    ("Brave New World", "Aldous Huxley", "9780060850524", 1932, 1),
    ("Fahrenheit 451", "Ray Bradbury", "9781451673319", 1953, 2),
    ("Clean Code", "Robert C. Martin", "9780132350884", 2008, 2),
    (
        "The Pragmatic Programmer",
        "David Thomas and Andrew Hunt",
        "9780135957059",
        2019,
        1,
    ),
    (
        "Design Patterns",
        "Erich Gamma et al.",
        "9780201633610",
        1994,
        1,
    ),
    (
        "Structure and Interpretation of Computer Programs",
        "Harold Abelson and Gerald Jay Sussman",
        "9780262510875",
        1996,
        2,
    ),
    (
        "The Rust Programming Language",
        "Steve Klabnik and Carol Nichols",
        "9781718503106",
        2023,
        3,
    ),
];

/// `(name, email)`
const MEMBERS: [(&str, &str); 6] = [
    ("Ada Lovelace", "ada@example.com"),
    ("Alan Turing", "alan@example.com"),
    ("Grace Hopper", "grace@example.com"),
    ("Margaret Hamilton", "margaret@example.com"),
    ("Edsger Dijkstra", "edsger@example.com"),
    ("Barbara Liskov", "barbara@example.com"),
];

/// `(book index, member index, days ago it was borrowed)`. Loans last 14 days,
/// so the first one (20 days ago) is overdue by 6 days.
const LOANS: [(usize, usize, i64); 3] = [(0, 0, 20), (4, 1, 3), (7, 2, 1)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedReport {
    Seeded {
        books: usize,
        members: usize,
        loans: usize,
    },
    /// Books or members already exist, so nothing was inserted.
    Skipped,
}

/// Insert the sample data if (and only if) there are no books and no members.
/// Running it again is therefore a no-op.
///
/// Seeding is not atomic: if it fails part-way, the rows inserted so far stay
/// and a re-run will skip. To reset, delete the database file and run again. `now` is passed in (rather than read
/// from the system) so the loan dates are testable.
pub async fn seed(pool: &SqlitePool, now: DateTime<Utc>) -> anyhow::Result<SeedReport> {
    let books = SqliteBookRepository::new(pool.clone());
    let members = SqliteMemberRepository::new(pool.clone());
    let loans = SqliteLoanRepository::new(pool.clone());

    let one = PageParams::new(Some(1), Some(1));
    let (_, book_count) = books
        .list(
            &BookFilter {
                q: None,
                availability: Availability::All,
            },
            one,
            None,
        )
        .await?;
    let (_, member_count) = members.list(&MemberFilter { q: None }, one, None).await?;
    if book_count > 0 || member_count > 0 {
        return Ok(SeedReport::Skipped);
    }

    let mut book_ids = Vec::new();
    for (title, author, isbn, year, copies) in BOOKS {
        let input = BookInput {
            title: title.to_owned(),
            author: author.to_owned(),
            isbn: Some(isbn.to_owned()),
            published_year: Some(year),
            total_copies: copies,
        };
        let fields = BookFields::parse(input, now.year()).map_err(|errors| {
            anyhow::anyhow!("invalid seed book `{title}`: {}", readable(&errors))
        })?;
        book_ids.push(books.create(&fields, now).await?.id);
    }

    let mut member_ids = Vec::new();
    for (name, email) in MEMBERS {
        let input = MemberInput {
            name: name.to_owned(),
            email: email.to_owned(),
        };
        let fields = MemberFields::parse(input).map_err(|errors| {
            anyhow::anyhow!("invalid seed member `{name}`: {}", readable(&errors))
        })?;
        member_ids.push(members.create(&fields, now).await?.id);
    }

    for (book, member, days_ago) in LOANS {
        let borrowed_at = now - Duration::days(days_ago);
        let record = NewLoanRecord {
            book_id: *book_ids
                .get(book)
                .context("seed loan names a missing book")?,
            member_id: *member_ids
                .get(member)
                .context("seed loan names a missing member")?,
            borrowed_at,
            due_at: borrowed_at + Duration::days(LOAN_PERIOD_DAYS),
        };
        match loans
            .create_checked(record, MAX_ACTIVE_LOANS_PER_MEMBER)
            .await?
        {
            CreateLoanOutcome::Created(_) => {}
            other => bail!("seed loan was refused: {other:?}"),
        }
    }

    Ok(SeedReport::Seeded {
        books: book_ids.len(),
        members: member_ids.len(),
        loans: LOANS.len(),
    })
}

/// `field message; field message`, for error output.
fn readable(errors: &FieldErrors) -> String {
    errors
        .iter()
        .map(|(field, message)| format!("{field} {message}"))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use library_shared::LoanStatus;

    use super::*;
    use crate::books::Isbn;
    use crate::loans::LoanFilter;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    /// A typo in a seed ISBN must fail here, not at the first `seed` run.
    #[test]
    fn seed_isbns_are_valid_and_unique() {
        for (title, _, isbn, _, _) in BOOKS {
            assert!(Isbn::parse(isbn).is_ok(), "bad ISBN for {title}: {isbn}");
        }
        let mut isbns: Vec<_> = BOOKS.iter().map(|b| b.2).collect();
        isbns.sort_unstable();
        isbns.dedup();
        assert_eq!(isbns.len(), BOOKS.len());
    }

    #[sqlx::test(migrations = "../../migrations/sqlite")]
    async fn seeds_once_then_skips(pool: SqlitePool) {
        let first = seed(&pool, now()).await.unwrap();
        assert_eq!(
            first,
            SeedReport::Seeded {
                books: 12,
                members: 6,
                loans: 3
            }
        );
        let second = seed(&pool, now()).await.unwrap();
        assert_eq!(second, SeedReport::Skipped);

        for (sql, expected) in [
            ("SELECT COUNT(*) FROM books", 12),
            ("SELECT COUNT(*) FROM members", 6),
            ("SELECT COUNT(*) FROM loans", 3),
        ] {
            let n: i64 = sqlx::query_scalar(sql).fetch_one(&pool).await.unwrap();
            assert_eq!(n, expected, "{sql}");
        }
    }

    #[sqlx::test(migrations = "../../migrations/sqlite")]
    async fn exactly_one_seeded_loan_is_overdue(pool: SqlitePool) {
        seed(&pool, now()).await.unwrap();
        let loans = SqliteLoanRepository::new(pool);
        let filter = |status| LoanFilter {
            status,
            member_id: None,
            book_id: None,
        };
        let page = PageParams::new(None, None);
        let (overdue, _) = loans
            .list(&filter(LoanStatus::Overdue), now(), page, None)
            .await
            .unwrap();
        assert_eq!(overdue.len(), 1);
        assert_eq!(overdue[0].due_at, now() - Duration::days(6));
        let (active, _) = loans
            .list(&filter(LoanStatus::Active), now(), page, None)
            .await
            .unwrap();
        assert_eq!(active.len(), 3);
    }

    #[sqlx::test(migrations = "../../migrations/sqlite")]
    async fn existing_data_is_left_alone(pool: SqlitePool) {
        sqlx::query("INSERT INTO members (name, email, created_at, updated_at) VALUES ('M', 'm@x.io', 0, 0)")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(seed(&pool, now()).await.unwrap(), SeedReport::Skipped);
        let books: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM books")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(books, 0);
    }
}
