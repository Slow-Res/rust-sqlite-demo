# /api/books. Reads are public; writes require a librarian.
class BooksController < ApplicationController
  before_action :require_user!, only: %i[create update destroy]

  def index
    # Everything is read from the query string first (a bad value is a 400), then
    # the cursor is decoded and the search term checked.
    page, per_page, after = query_uint32("page"), query_uint32("per_page"), query_string("after")
    availability = query_enum("availability", Book::AVAILABILITIES) || "all"
    q = query_string("q")
    page_params, cursor = Pagination.params(page, per_page, after, :string)
    filter = Book::Filter.new(q: Pagination.search_term(q), availability: availability)

    books, total = Book.list(filter, page_params, cursor)
    render json: Pagination.envelope(page_params, books, total, cursor_key: :title.to_proc, &:as_api)
  end

  def show
    render json: find_book(path_id).as_api
  end

  def create
    book = book_from_body(json_body)
    book.validate_input!
    book.save!(validate: false)
    render json: Book.find_with_availability(book.id).as_api, status: :created
  end

  def update
    id = path_id
    candidate = book_from_body(json_body)
    candidate.validate_input!
    case (result = Book.update_checked(id, candidate, Clock.now))
    when :not_found then raise ApiError.not_found("book")
    when :copies_below_active_loans
      raise ApiError.conflict("copies_below_active_loans",
                              "total copies cannot be less than the copies currently on loan")
    else render json: result.as_api
    end
  end

  def destroy
    case Book.delete_checked(path_id)
    when :deleted then head :no_content
    when :not_found then raise ApiError.not_found("book")
    when :has_active_loans
      raise ApiError.conflict("book_has_active_loans", "book has active loans and cannot be deleted")
    end
  end

  private

  def find_book(id)
    Book.find_with_availability(id) || raise(ApiError.not_found("book"))
  end

  def book_from_body(body)
    book = Book.from_input(title: body_string(body, "title"), author: body_string(body, "author"),
                           isbn: body_string(body, "isbn", optional: true),
                           published_year: body_integer(body, "published_year", INT32, optional: true),
                           total_copies: body_integer(body, "total_copies", INT64))
    book.created_at = book.updated_at = Clock.now
    book
  end
end
