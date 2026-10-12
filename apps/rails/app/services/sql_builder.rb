# Dynamic SQL with numbered placeholders ($1, $2, ...) and typed binds, run as a
# prepared statement: what sqlx's QueryBuilder does in the Rust app. ActiveRecord's
# own `where(...)` and `find_by_sql([sql, *values])` would put the values into the
# SQL text, which makes a new statement for every value.
class SqlBuilder
  INTEGER = ActiveModel::Type::Integer.new(limit: 8)
  STRING = ActiveModel::Type::String.new

  attr_reader :sql, :binds

  def initialize(sql = "")
    @sql = +sql
    @binds = []
  end

  def <<(fragment)
    @sql << fragment
    self
  end

  # Append the next placeholder and remember `value` (an Integer or a String) for it.
  def bind(value)
    type = value.is_a?(Integer) ? INTEGER : STRING
    @binds << ActiveRecord::Relation::QueryAttribute.new("$#{@binds.size + 1}", value, type)
    @sql << "$#{@binds.size}"
    self
  end

  # The rows as `model` records (extra selected columns become attributes).
  def records(model)
    model.find_by_sql(@sql, @binds, preparable: true)
  end

  # The first column of the first row.
  def scalar
    ApplicationRecord.lease_connection.select_all(@sql, "SQL", @binds, preparable: true).rows.dig(0, 0)
  end
end
