# A library member, with the derived `active_loans` (unreturned loans) when read
# through the queries below. The queries are the ones of
# apps/web/src/members/postgres.rs.
class Member < ApplicationRecord
  COLUMNS = "SELECT m.id, m.name, m.email, " \
            "(SELECT COUNT(*) FROM loans l WHERE l.member_id = m.id AND l.returned_at IS NULL) AS active_loans, " \
            "m.created_at, m.updated_at".freeze

  validates :name, presence: { message: "must not be empty" },
                   length: { maximum: 200, message: "must be at most 200 characters" }
  attr_accessor :email_error

  validate { errors.add(:email, email_error) if email_error }

  # A member (not saved) from a request body: name trimmed, email trimmed and lowercased.
  def self.from_input(name:, email:)
    normalized, error = Email.parse(email)
    new(name: Text.trim(name), email: normalized).tap { |member| member.email_error = error }
  end

  def self.find_with_active_loans(id)
    SqlBuilder.new("#{COLUMNS} FROM members m WHERE m.id = ").bind(id).records(self).first
  end

  # `[members, total]`: up to `params.limit` matching members and the number of
  # matches. With `after` (`[name, id]` of the last member seen) the rows start
  # strictly after it.
  def self.list(q, params, after)
    total = CountCache.shared.get_or_count(q.nil? ? :members : nil) { count_matching(q) }

    # Deferred join: the inner query walks the index and skips OFFSET rows reading
    # only ids; the columns and the correlated subquery are computed for the
    # page's rows only.
    sql = SqlBuilder.new("#{COLUMNS} FROM (SELECT m.id FROM members m")
    push_filter(sql, q)
    if after
      sql << " AND (m.name, m.id) > ("
      sql.bind(after[0]) << ", "
      sql.bind(after[1]) << ")"
    end
    sql << " ORDER BY m.name ASC, m.id ASC LIMIT "
    sql.bind(params.limit) << " OFFSET "
    sql.bind(params.offset) << ") p JOIN members m ON m.id = p.id ORDER BY m.name ASC, m.id ASC"
    [sql.records(self), total]
  end

  def self.count_matching(q)
    sql = SqlBuilder.new("SELECT COUNT(*) FROM members m")
    push_filter(sql, q)
    sql.scalar
  end

  # `ILIKE` under the "C" collation folds ASCII case only, like SQLite's LIKE.
  def self.push_filter(sql, q)
    sql << " WHERE 1 = 1"
    return unless q

    pattern = "%#{Text.escape_like(q)}%"
    sql << " AND (m.name ILIKE "
    sql.bind(pattern) << " ESCAPE '\\' OR m.email ILIKE "
    sql.bind(pattern) << " ESCAPE '\\')"
  end
  private_class_method :push_filter

  # Replace name and email of member `id` with those of `candidate` (a validated
  # from_input member); false if the member does not exist.
  def self.update_fields(id, candidate, now)
    # A single UPDATE is atomic on its own; zero affected rows: no such member.
    where(id: id).update_all(name: candidate.name, email: candidate.email, updated_at: now).positive?
  end

  # Delete a member (and their returned loans) unless they have active loans.
  # Returns :deleted, :not_found or :has_active_loans.
  def self.delete_checked(id)
    # Under READ COMMITTED a loan could be created between the count and the
    # delete. The lock on the member row makes Loan.borrow (which locks the member
    # too) wait. Lock order everywhere: book, member, loan.
    transaction do
      member = lock.find_by(id: id)
      next :not_found unless member
      next :has_active_loans if Loan.where(member_id: id, returned_at: nil).count.positive?

      # Returned loans disappear with the member (ON DELETE CASCADE).
      member.destroy!
      :deleted
    end
  end

  def as_api
    { id: id, name: name, email: email, active_loans: self[:active_loans],
      created_at: Clock.iso(created_at), updated_at: Clock.iso(updated_at) }
  end
end
