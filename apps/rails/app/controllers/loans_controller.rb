# /api/loans. Every endpoint requires a librarian.
class LoansController < ApplicationController
  before_action :require_user!

  def index
    status = query_enum("status", Loan::STATUSES) || "active"
    member_id, book_id = query_int64("member_id"), query_int64("book_id")
    page, per_page, after = query_uint32("page"), query_uint32("per_page"), query_string("after")
    page_params, cursor = Pagination.params(page, per_page, after, :integer)
    filter = Loan::Filter.new(status: status, member_id: member_id, book_id: book_id)

    now = Clock.now
    loans, total = Loan.list(filter, now, page_params, cursor)
    # The cursor holds the key the list is ordered by.
    key = ->(loan) { filter.returned? ? (loan.returned_at || loan.due_at) : loan.due_at }
    render json: Pagination.envelope(page_params, loans, total, cursor_key: key) { |loan| loan.as_api(now) }
  end

  # Borrow a book: POST /api/loans {"book_id": 1, "member_id": 2}.
  def create
    body = json_body
    book_id = body_integer(body, "book_id", INT64)
    member_id = body_integer(body, "member_id", INT64)
    errors = {}
    errors["book_id"] = "must be a positive id" if book_id < 1
    errors["member_id"] = "must be a positive id" if member_id < 1
    raise ApiError.validation(errors) if errors.any?

    now = Clock.now
    case (result = Loan.borrow(book_id, member_id, now))
    when :book_not_found then raise ApiError.not_found("book")
    when :member_not_found then raise ApiError.not_found("member")
    when :no_copies_available then raise ApiError.conflict("no_copies_available", "no copies of this book are available")
    when :loan_limit_reached
      raise ApiError.conflict("loan_limit_reached",
                              "member already has the maximum of #{Loan::MAX_ACTIVE_PER_MEMBER} active loans")
    else render json: result.as_api(now), status: :created
    end
  end

  def return_loan
    id = path_id
    now = Clock.now
    case (result = Loan.mark_returned(id, now))
    when :not_found then raise ApiError.not_found("loan")
    when :already_returned then raise ApiError.conflict("loan_already_returned", "loan has already been returned")
    else render json: result.as_api(now)
    end
  end
end
