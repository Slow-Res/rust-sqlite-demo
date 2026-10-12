# A login. The primary key is the hex SHA-256 of the raw token (the raw token
# only ever lives in the cookie).
class UserSession < ApplicationRecord
  self.table_name = "sessions"
  self.primary_key = "token_hash"

  belongs_to :user

  # `[id, username]` of the user of an unexpired session, or nil.
  def self.user_for(token_hash, now)
    # expires_at > now, written as >= now + 1 so that it is a bound value (the
    # seconds are integers, so it is the same test).
    User.joins(:sessions).where(sessions: { token_hash: token_hash, expires_at: (now + 1).. })
        .pick(:id, :username)
  end

  # Sweep: most logins find nothing to delete, so look before writing.
  def self.delete_expired(now)
    where(expires_at: ..now).delete_all if where(expires_at: ..now).exists?
  end
end
