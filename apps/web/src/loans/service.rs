//! Loan business rules: due dates, overdue, input checks, outcome-to-error mapping.
//! Knows nothing about HTTP or SQL.

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use library_shared::{Loan, LoanQuery, LoanStatus, NewLoan, Page};

use super::repo::{
    CreateLoanOutcome, LoanDetails, LoanFilter, LoanRepository, NewLoanRecord, ReturnLoanOutcome,
};
use super::{LOAN_PERIOD_DAYS, MAX_ACTIVE_LOANS_PER_MEMBER};
use crate::clock::Clock;
use crate::error::{AppError, FieldErrors};
use crate::pagination::PageParams;

/// The service holds its collaborators as trait objects (`Arc<dyn ...>`):
/// production passes the SQLite repository and the system clock, the unit
/// tests below pass an in-memory fake and a fixed clock. That is what the
/// port/adapter split buys: these rules are tested without a database.
#[derive(Clone)]
pub struct LoanService {
    repo: Arc<dyn LoanRepository>,
    clock: Arc<dyn Clock>,
}

impl LoanService {
    pub fn new(repo: Arc<dyn LoanRepository>, clock: Arc<dyn Clock>) -> Self {
        Self { repo, clock }
    }

    pub async fn list(&self, query: LoanQuery) -> Result<Page<Loan>, AppError> {
        let page = PageParams::new(query.page, query.per_page);
        let filter = LoanFilter {
            status: query.status.unwrap_or(LoanStatus::Active),
            member_id: query.member_id,
            book_id: query.book_id,
        };
        let now = self.clock.now();
        let (records, total) = self.repo.list(&filter, now, page).await?;
        let items = records.into_iter().map(|r| to_loan(r, now)).collect();
        Ok(page.into_page(items, total))
    }

    /// Borrow a book. The service owns the *policy* (14-day period, limit of 5);
    /// the repository enforces it atomically and reports what happened.
    pub async fn create(&self, input: NewLoan) -> Result<Loan, AppError> {
        let mut errors = FieldErrors::new();
        if input.book_id < 1 {
            errors.add("book_id", "must be a positive id");
        }
        if input.member_id < 1 {
            errors.add("member_id", "must be a positive id");
        }
        errors.into_result()?;

        let now = self.clock.now();
        let record = NewLoanRecord {
            book_id: input.book_id,
            member_id: input.member_id,
            borrowed_at: now,
            due_at: now + Duration::days(LOAN_PERIOD_DAYS),
        };
        match self
            .repo
            .create_checked(record, MAX_ACTIVE_LOANS_PER_MEMBER)
            .await?
        {
            CreateLoanOutcome::Created(details) => Ok(to_loan(details, now)),
            CreateLoanOutcome::BookNotFound => Err(AppError::NotFound("book")),
            CreateLoanOutcome::MemberNotFound => Err(AppError::NotFound("member")),
            CreateLoanOutcome::NoCopiesAvailable => Err(AppError::Conflict {
                code: "no_copies_available",
                message: "no copies of this book are available".to_owned(),
            }),
            CreateLoanOutcome::LoanLimitReached => Err(AppError::Conflict {
                code: "loan_limit_reached",
                message: format!(
                    "member already has the maximum of {MAX_ACTIVE_LOANS_PER_MEMBER} active loans"
                ),
            }),
        }
    }

    pub async fn return_loan(&self, id: i64) -> Result<Loan, AppError> {
        let now = self.clock.now();
        match self.repo.mark_returned(id, now).await? {
            ReturnLoanOutcome::Returned(details) => Ok(to_loan(details, now)),
            ReturnLoanOutcome::NotFound => Err(AppError::NotFound("loan")),
            ReturnLoanOutcome::AlreadyReturned => Err(AppError::Conflict {
                code: "loan_already_returned",
                message: "loan has already been returned".to_owned(),
            }),
        }
    }
}

/// Overdue means unreturned and strictly past due: a loan due exactly `now`
/// is not overdue yet. The SQL `overdue` filter uses the same rule.
fn is_overdue(details: &LoanDetails, now: DateTime<Utc>) -> bool {
    details.returned_at.is_none() && details.due_at < now
}

fn to_loan(details: LoanDetails, now: DateTime<Utc>) -> Loan {
    Loan {
        is_overdue: is_overdue(&details, now),
        id: details.id,
        book_id: details.book_id,
        book_title: details.book_title,
        member_id: details.member_id,
        member_name: details.member_name,
        borrowed_at: details.borrowed_at,
        due_at: details.due_at,
        returned_at: details.returned_at,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::clock::FixedClock;
    use crate::db::RepoError;

    /// An in-memory [`LoanRepository`]: no database, same observable rules.
    /// It exists to show the point of the port: the service can be tested
    /// against anything that implements the trait.
    #[derive(Default)]
    struct FakeLoanRepository {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        /// book id -> (title, total copies)
        books: HashMap<i64, (String, i64)>,
        /// member id -> name
        members: HashMap<i64, String>,
        loans: Vec<LoanDetails>,
    }

    impl FakeLoanRepository {
        fn with(books: &[(i64, i64)], members: &[i64]) -> Arc<Self> {
            let fake = Self::default();
            {
                let mut state = fake.lock();
                for &(id, copies) in books {
                    state.books.insert(id, (format!("Book {id}"), copies));
                }
                for &id in members {
                    state.members.insert(id, format!("Member {id}"));
                }
            }
            Arc::new(fake)
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.state.lock().unwrap()
        }
    }

    #[async_trait]
    impl LoanRepository for FakeLoanRepository {
        async fn list(
            &self,
            filter: &LoanFilter,
            now: DateTime<Utc>,
            page: PageParams,
        ) -> Result<(Vec<LoanDetails>, u64), RepoError> {
            let matching: Vec<_> = self
                .lock()
                .loans
                .iter()
                .filter(|l| match filter.status {
                    LoanStatus::Active => l.returned_at.is_none(),
                    LoanStatus::Overdue => l.returned_at.is_none() && l.due_at < now,
                    LoanStatus::Returned => l.returned_at.is_some(),
                    LoanStatus::All => true,
                })
                .cloned()
                .collect();
            let total = matching.len() as u64;
            let items = matching
                .into_iter()
                .take(page.per_page() as usize)
                .collect();
            Ok((items, total))
        }

        async fn get(&self, id: i64) -> Result<Option<LoanDetails>, RepoError> {
            Ok(self.lock().loans.iter().find(|l| l.id == id).cloned())
        }

        async fn create_checked(
            &self,
            loan: NewLoanRecord,
            max_active_per_member: i64,
        ) -> Result<CreateLoanOutcome, RepoError> {
            let mut state = self.lock();
            let Some((title, total)) = state.books.get(&loan.book_id).cloned() else {
                return Ok(CreateLoanOutcome::BookNotFound);
            };
            let Some(name) = state.members.get(&loan.member_id).cloned() else {
                return Ok(CreateLoanOutcome::MemberNotFound);
            };
            let active = |pred: &dyn Fn(&LoanDetails) -> bool| {
                state
                    .loans
                    .iter()
                    .filter(|l| l.returned_at.is_none() && pred(l))
                    .count() as i64
            };
            if active(&|l| l.book_id == loan.book_id) >= total {
                return Ok(CreateLoanOutcome::NoCopiesAvailable);
            }
            if active(&|l| l.member_id == loan.member_id) >= max_active_per_member {
                return Ok(CreateLoanOutcome::LoanLimitReached);
            }
            let details = LoanDetails {
                id: state.loans.len() as i64 + 1,
                book_id: loan.book_id,
                book_title: title,
                member_id: loan.member_id,
                member_name: name,
                borrowed_at: loan.borrowed_at,
                due_at: loan.due_at,
                returned_at: None,
            };
            state.loans.push(details.clone());
            Ok(CreateLoanOutcome::Created(details))
        }

        async fn mark_returned(
            &self,
            id: i64,
            returned_at: DateTime<Utc>,
        ) -> Result<ReturnLoanOutcome, RepoError> {
            let mut state = self.lock();
            let Some(loan) = state.loans.iter_mut().find(|l| l.id == id) else {
                return Ok(ReturnLoanOutcome::NotFound);
            };
            if loan.returned_at.is_some() {
                return Ok(ReturnLoanOutcome::AlreadyReturned);
            }
            loan.returned_at = Some(returned_at);
            Ok(ReturnLoanOutcome::Returned(loan.clone()))
        }
    }

    fn start() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn setup(books: &[(i64, i64)], members: &[i64]) -> (LoanService, Arc<FixedClock>) {
        let clock = Arc::new(FixedClock::new(start()));
        let repo = FakeLoanRepository::with(books, members);
        (LoanService::new(repo, clock.clone()), clock)
    }

    fn new_loan(book_id: i64, member_id: i64) -> NewLoan {
        NewLoan { book_id, member_id }
    }

    fn conflict_code(err: AppError) -> &'static str {
        match err {
            AppError::Conflict { code, .. } => code,
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn due_date_is_exactly_fourteen_days_after_now() {
        let (service, _) = setup(&[(1, 1)], &[1]);
        let loan = service.create(new_loan(1, 1)).await.unwrap();
        assert_eq!(loan.borrowed_at, start());
        assert_eq!(loan.due_at, start() + Duration::days(14));
        assert_eq!(loan.book_title, "Book 1");
        assert_eq!(loan.member_name, "Member 1");
        assert!(!loan.is_overdue);
        assert_eq!(loan.returned_at, None);
    }

    #[tokio::test]
    async fn non_positive_ids_are_validation_errors() {
        let (service, _) = setup(&[], &[]);
        let Err(AppError::Validation(fields)) = service.create(new_loan(0, -3)).await else {
            panic!("expected a validation error");
        };
        let fields: Vec<_> = fields.iter().collect();
        assert_eq!(
            fields,
            [
                ("book_id", "must be a positive id"),
                ("member_id", "must be a positive id")
            ]
        );
    }

    #[tokio::test]
    async fn unknown_book_and_member_are_not_found() {
        let (service, _) = setup(&[(1, 1)], &[1]);
        let err = service.create(new_loan(9, 1)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound("book")));
        let err = service.create(new_loan(1, 9)).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound("member")));
    }

    #[tokio::test]
    async fn last_copy_cannot_be_borrowed_twice() {
        let (service, _) = setup(&[(1, 1)], &[1, 2]);
        service.create(new_loan(1, 1)).await.unwrap();
        let err = service.create(new_loan(1, 2)).await.unwrap_err();
        assert_eq!(conflict_code(err), "no_copies_available");
    }

    #[tokio::test]
    async fn sixth_active_loan_is_refused() {
        let books: Vec<_> = (1..=6).map(|id| (id, 1)).collect();
        let (service, _) = setup(&books, &[1]);
        for book in 1..=5 {
            service.create(new_loan(book, 1)).await.unwrap();
        }
        let err = service.create(new_loan(6, 1)).await.unwrap_err();
        match err {
            AppError::Conflict { code, message } => {
                assert_eq!(code, "loan_limit_reached");
                assert_eq!(message, "member already has the maximum of 5 active loans");
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn return_maps_each_outcome() {
        let (service, clock) = setup(&[(1, 1)], &[1]);
        let loan = service.create(new_loan(1, 1)).await.unwrap();

        clock.advance(Duration::days(3));
        let returned = service.return_loan(loan.id).await.unwrap();
        assert_eq!(returned.returned_at, Some(start() + Duration::days(3)));

        let err = service.return_loan(loan.id).await.unwrap_err();
        assert_eq!(conflict_code(err), "loan_already_returned");
        let err = service.return_loan(99).await.unwrap_err();
        assert!(matches!(err, AppError::NotFound("loan")));
    }

    #[tokio::test]
    async fn overdue_starts_strictly_after_the_due_date() {
        let (service, clock) = setup(&[(1, 1)], &[1]);
        service.create(new_loan(1, 1)).await.unwrap();
        let is_overdue = |service: &LoanService| {
            let service = service.clone();
            async move { service.list(LoanQuery::default()).await.unwrap().items[0].is_overdue }
        };

        clock.set(start() + Duration::days(14));
        assert!(!is_overdue(&service).await, "due_at == now is not overdue");
        clock.set(start() + Duration::days(14) + Duration::seconds(1));
        assert!(is_overdue(&service).await);
    }

    #[tokio::test]
    async fn returned_loans_are_never_overdue() {
        let (service, clock) = setup(&[(1, 1)], &[1]);
        let loan = service.create(new_loan(1, 1)).await.unwrap();
        clock.advance(Duration::days(30));
        let returned = service.return_loan(loan.id).await.unwrap();
        assert!(!returned.is_overdue);
    }
}
