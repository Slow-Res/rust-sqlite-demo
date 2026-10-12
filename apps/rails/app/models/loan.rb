# A loan: a member holds a book from `borrowed_at` to `due_at`, until `returned_at`.
# The queries are the ones of apps/web/src/loans/postgres.rs.
class Loan < ApplicationRecord
  LOAN_PERIOD_DAYS = 14
  MAX_ACTIVE_PER_MEMBER = 5
  STATUSES = %w[active overdue returned all].freeze

  # A loan joined with the title of its book and the name of its member.
  DETAILS = "SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name, " \
            "l.borrowed_at, l.due_at, l.returned_at " \
            "FROM loans l JOIN books b ON b.id = l.book_id JOIN members m ON m.id = l.member_id".freeze

  Filter = Data.define(:status, :member_id, :book_id) do
    # The count-cache slot, if the total is cacheable: a status of active, returned
    # or all with no member or book. `overdue` depends on the current time.
    def count_key
      return nil if member_id || book_id

      { "active" => :loans_active, "returned" => :loans_returned, "all" => :loans_all }[status]
    end

    def returned? = status == "returned"
  end

  def self.find_details(id)
    SqlBuilder.new("#{DETAILS} WHERE l.id = ").bind(id).records(self).first
  end

  # `[loans, total]`: up to `params.limit` matching loans and the number of matches.
  # The order is `due_at, id`, or `returned_at DESC, id DESC` for returned loans;
  # `after` is `[key, id]` of the last loan seen in that order.
  def self.list(filter, now, params, after)
    total = CountCache.shared.get_or_count(filter.count_key) { count_matching(filter, now) }

    # Deferred join: the inner query walks the index and skips OFFSET rows reading
    # only ids; the joins to books and members are done for the page's rows only.
    order = filter.returned? ? " ORDER BY l.returned_at DESC, l.id DESC" : " ORDER BY l.due_at ASC, l.id ASC"
    sql = SqlBuilder.new("SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name, " \
                         "l.borrowed_at, l.due_at, l.returned_at FROM (SELECT l.id FROM loans l")
    push_filter(sql, filter, now)
    if after
      # Strictly after the cursor in the list's own direction.
      sql << (filter.returned? ? " AND (l.returned_at, l.id) < (" : " AND (l.due_at, l.id) > (")
      sql.bind(after[0]) << ", "
      sql.bind(after[1]) << ")"
    end
    sql << order << " LIMIT "
    sql.bind(params.limit) << " OFFSET "
    sql.bind(params.offset) << ") p JOIN loans l ON l.id = p.id JOIN books b ON b.id = l.book_id " \
                               "JOIN members m ON m.id = l.member_id" << order
    [sql.records(self), total]
  end

  def self.count_matching(filter, now)
    sql = SqlBuilder.new("SELECT COUNT(*) FROM loans l")
    push_filter(sql, filter, now)
    sql.scalar
  end

  # `overdue` is unreturned and `due_at < now`, the same rule as `overdue?`.
  def self.push_filter(sql, filter, now)
    sql << " WHERE 1 = 1"
    case filter.status
    when "active" then sql << " AND l.returned_at IS NULL"
    when "overdue"
      sql << " AND l.returned_at IS NULL AND l.due_at < "
      sql.bind(now)
    when "returned" then sql << " AND l.returned_at IS NOT NULL"
    end
    if filter.member_id
      sql << " AND l.member_id = "
      sql.bind(filter.member_id)
    end
    return unless filter.book_id

    sql << " AND l.book_id = "
    sql.bind(filter.book_id)
  end
  private_class_method :push_filter

  # Borrow a book: check book, member, free copies and the member's limit, then
  # insert. Returns the loan (with details) or :book_not_found, :member_not_found,
  # :no_copies_available, :loan_limit_reached; the first failing check wins.
  def self.borrow(book_id, member_id, now)
    # Check-then-insert must be atomic, or two simultaneous requests could both
    # take the last copy. READ COMMITTED does not serialize writers, so the rows
    # the checks depend on are locked with FOR UPDATE before counting: the book
    # (copies free) and the member (loan limit). A competing borrow blocks on the
    # lock, then its counts include our committed loan. Lock order everywhere:
    # book, member, loan, so transactions cannot deadlock.
    transaction do
      total_copies = Book.lock.where(id: book_id).pick(:total_copies)
      next :book_not_found unless total_copies
      next :member_not_found unless Member.lock.where(id: member_id).pick(:id)
      next :no_copies_available if where(book_id: book_id, returned_at: nil).count >= total_copies
      next :loan_limit_reached if where(member_id: member_id, returned_at: nil).count >= MAX_ACTIVE_PER_MEMBER

      loan = create!(book_id: book_id, member_id: member_id, borrowed_at: now,
                     due_at: now + (LOAN_PERIOD_DAYS * 86_400))
      find_details(loan.id)
    end
  end

  # Mark a loan returned. Returns the loan (with details), :not_found or
  # :already_returned.
  def self.mark_returned(id, now)
    transaction do
      # Lock order is book, member, loan, but the loan tells us which book and
      # member those are, so read it unlocked first.
      book_id, member_id = where(id: id).pick(:book_id, :member_id)
      next :not_found unless book_id

      Book.lock.where(id: book_id).pick(:id)
      Member.lock.where(id: member_id).pick(:id)

      # Now lock the loan and decide on its current state. It is gone if a
      # concurrent delete of its book or member removed it in the meantime.
      loan = lock.find_by(id: id)
      next :not_found unless loan
      next :already_returned if loan.returned_at

      loan.update!(returned_at: now)
      find_details(id)
    end
  end

  # Overdue means unreturned and strictly past due: a loan due exactly now is not
  # overdue yet.
  def overdue?(now) = returned_at.nil? && due_at < now

  def as_api(now)
    { id: id, book_id: book_id, book_title: self[:book_title], member_id: member_id,
      member_name: self[:member_name], borrowed_at: Clock.iso(borrowed_at), due_at: Clock.iso(due_at),
      returned_at: Clock.iso(returned_at), is_overdue: overdue?(now) }
  end
end
