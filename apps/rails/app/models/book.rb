# A book, with the derived `available_copies` (total_copies minus unreturned
# loans) when it is read through the queries below. The list queries are the ones
# of apps/web/src/books/postgres.rs.
class Book < ApplicationRecord
  # Number of unreturned loans of the book aliased `b`.
  ACTIVE_LOANS = "(SELECT COUNT(*) FROM loans l WHERE l.book_id = b.id AND l.returned_at IS NULL)".freeze

  COLUMNS = "SELECT b.id, b.title, b.author, b.isbn, b.published_year, b.total_copies, " \
            "b.total_copies - #{ACTIVE_LOANS} AS available_copies, b.created_at, b.updated_at".freeze

  # The totals of an availability-only filter, from the active loans: the per-book
  # predicate is a correlated subquery for every book. A book with no active loan
  # is available (total_copies is at least 1, CHECK), and a book is full when its
  # active loans use every copy.
  ON_LOAN_TOTAL = "SELECT COUNT(DISTINCT book_id) FROM loans WHERE returned_at IS NULL".freeze
  FULL_BOOKS_TOTAL = "SELECT COUNT(*) FROM (SELECT book_id, COUNT(*) AS n " \
                     "FROM loans WHERE returned_at IS NULL GROUP BY book_id) a " \
                     "JOIN books b ON b.id = a.book_id WHERE b.total_copies - a.n <= 0".freeze

  AVAILABILITIES = %w[all available on_loan].freeze

  # Search criteria. `q` is already trimmed (nil: no text search).
  Filter = Data.define(:q, :availability) do
    # The count-cache slot, if the total is cacheable: the unfiltered list and a
    # list filtered by availability alone.
    def count_key
      return nil if q

      { "all" => :books, "available" => :books_available, "on_loan" => :books_on_loan }.fetch(availability)
    end

    # Whether the total can come from the active loans alone.
    def counts_from_loans? = q.nil? && availability != "all"
  end

  validates :title, presence: { message: "must not be empty" },
                    length: { maximum: 300, message: "must be at most 300 characters" }
  validates :author, presence: { message: "must not be empty" },
                     length: { maximum: 200, message: "must be at most 200 characters" }
  validates :total_copies, inclusion: { in: 1..1000, message: "must be between 1 and 1000" }
  validate :published_year_in_range
  attr_accessor :isbn_invalid

  validate { errors.add(:isbn, Isbn::INVALID) if isbn_invalid }

  # A book (not saved) from a request body, trimmed and normalized. Blank means
  # "no ISBN", not "invalid ISBN".
  def self.from_input(title:, author:, isbn:, published_year:, total_copies:)
    raw_isbn = isbn && Text.trim(isbn)
    normalized = raw_isbn.presence && Isbn.normalize(raw_isbn)
    new(title: Text.trim(title), author: Text.trim(author), isbn: normalized,
        published_year: published_year, total_copies: total_copies).tap do |book|
      book.isbn_invalid = raw_isbn.present? && normalized.nil?
    end
  end

  def self.find_with_availability(id)
    SqlBuilder.new("#{COLUMNS} FROM books b WHERE b.id = ").bind(id).records(self).first
  end

  # `[books, total]`: up to `params.limit` matching books and the number of
  # matches. With `after` (`[title, id]` of the last book seen) the rows start
  # strictly after it.
  def self.list(filter, params, after)
    # Deferred join: the inner query walks the index and skips OFFSET rows reading
    # only ids; the columns and the correlated subquery are computed for the
    # page's rows only.
    sql = SqlBuilder.new(COLUMNS)
    by_words = filter.q && Text.search_words(filter.q).any?
    if by_words
      # A word search finds its matches through the GIN index in a CTE that the
      # planner must not fold back into the title-order walk. The total comes from
      # the same statement: a window over the whole match set (so the cursor comes
      # after the window, or the total would only count the rows past it).
      sql << ", p.total FROM (WITH m AS MATERIALIZED (SELECT b.id, b.title FROM books b"
      push_filter(sql, filter)
      sql << ") SELECT id, title, total FROM (SELECT id, title, COUNT(*) OVER () AS total FROM m) x"
      if after
        sql << " WHERE (x.title, x.id) > ("
        sql.bind(after[0]) << ", "
        sql.bind(after[1]) << ")"
      end
      sql << " ORDER BY title ASC, id ASC"
    else
      sql << " FROM (SELECT b.id FROM books b"
      push_filter(sql, filter)
      if after
        sql << " AND (b.title, b.id) > ("
        sql.bind(after[0]) << ", "
        sql.bind(after[1]) << ")"
      end
      sql << " ORDER BY b.title ASC, b.id ASC"
    end
    sql << " LIMIT "
    sql.bind(params.limit) << " OFFSET "
    sql.bind(params.offset) << ") p JOIN books b ON b.id = p.id ORDER BY b.title ASC, b.id ASC"

    if by_words
      books = sql.records(self)
      return [books, books.first[:total]] if books.any?

      # An empty page (past the end, or no match) carries no window value: count.
      return [[], count_matching(filter)]
    end

    total = CountCache.shared.get_or_count(filter.count_key) { count_matching(filter) }
    [sql.records(self), total]
  end

  # Number of books matching `filter`.
  def self.count_matching(filter)
    # An availability filter without a search: count the active loans once.
    if filter.counts_from_loans?
      return SqlBuilder.new(ON_LOAN_TOTAL).scalar if filter.availability == "on_loan"

      # Available = all books - full books. Counting `books` is the expensive part,
      # and the unfiltered total is already cached for the list: reuse it. A book
      # deleted in between could make the difference negative: clamp it.
      all = CountCache.shared.get_or_count(:books) { count }
      return [all - SqlBuilder.new(FULL_BOOKS_TOTAL).scalar, 0].max
    end
    sql = SqlBuilder.new("SELECT COUNT(*) FROM books b")
    push_filter(sql, filter)
    sql.scalar
  end

  # The WHERE clause of `filter`, for the page and the count query. Every word of
  # `q` is a prefix of a word of title, author or ISBN. `book_search_vector` is the
  # expression of the GIN index idx_books_search; the ISBN form (`q` without
  # separators) is a LIKE prefix that the btree on isbn ("C" collation) serves.
  def self.push_filter(sql, filter)
    sql << " WHERE 1 = 1"
    if (q = filter.q)
      words = Text.search_words(q)
      if words.empty?
        # Nothing but punctuation: no word to look up, so match it as plain text in
        # a scan (rare).
        pattern = "%#{Text.escape_like(q)}%"
        sql << " AND (b.title ILIKE "
        sql.bind(pattern) << " ESCAPE '\\' OR b.author ILIKE "
        sql.bind(pattern) << " ESCAPE '\\' OR b.isbn ILIKE "
        sql.bind(pattern) << " ESCAPE '\\'"
        # Skipped when nothing is left, or `%%` would match all.
        digits = q.delete(" ").delete("-")
        unless digits.empty?
          sql << " OR b.isbn ILIKE "
          sql.bind("%#{Text.escape_like(digits)}%") << " ESCAPE '\\'"
        end
        sql << ")"
      else
        sql << " AND (book_search_vector(b.title, b.author, b.isbn) @@ to_tsquery('simple', "
        sql.bind(words.map { |word| "'#{word}':*" }.join(" & ")) << ")"
        if (digits = Text.isbn_prefix(q))
          # Letters and digits only, so no LIKE wildcard; ISBNs hold a capital X.
          sql << " OR b.isbn LIKE "
          sql.bind("#{digits.upcase(:ascii)}%")
        end
        sql << ")"
      end
    end
    case filter.availability
    when "available" then sql << " AND b.total_copies - #{ACTIVE_LOANS} > 0"
    when "on_loan" then sql << " AND #{ACTIVE_LOANS} > 0"
    end
  end
  private_class_method :push_filter

  # Replace all fields of book `id` with those of `candidate` (a validated
  # from_input book), unless that would leave fewer copies than active loans.
  # Returns the updated book, :not_found or :copies_below_active_loans.
  def self.update_checked(id, candidate, now)
    # The check (active loans vs. the new copy count) and the write must not be
    # separated by another writer, so the book row is locked first. Loans lock the
    # book before they count. Lock order everywhere: book, member, loan.
    transaction do
      book = lock.find_by(id: id)
      next :not_found unless book
      next :copies_below_active_loans if candidate.total_copies < Loan.where(book_id: id, returned_at: nil).count

      book.update!(candidate.attributes.slice("title", "author", "isbn", "published_year", "total_copies")
                            .merge("updated_at" => now))
      find_with_availability(id)
    end
  end

  # Delete a book (and its returned loans) unless it has active loans.
  # Returns :deleted, :not_found or :has_active_loans.
  def self.delete_checked(id)
    # Same lock as update_checked: no loan can be created while we count.
    transaction do
      book = lock.find_by(id: id)
      next :not_found unless book
      next :has_active_loans if Loan.where(book_id: id, returned_at: nil).count.positive?

      # Returned loans disappear with the book (ON DELETE CASCADE).
      book.destroy!
      :deleted
    end
  end

  def as_api
    { id: id, title: title, author: author, isbn: isbn, published_year: published_year,
      total_copies: total_copies, available_copies: self[:available_copies],
      created_at: Clock.iso(created_at), updated_at: Clock.iso(updated_at) }
  end

  private

  def published_year_in_range
    errors.add(:published_year, "must be a valid year") if published_year && !(1..Clock.year + 1).cover?(published_year)
  end
end
