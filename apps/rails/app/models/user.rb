# A librarian account. Created by the Rust app's `create-user` command; this app
# only reads users.
class User < ApplicationRecord
  has_many :sessions, class_name: "UserSession"

  # SQLite declares the column COLLATE NOCASE; here the unique index is on
  # lower(username) ("C" collation, ASCII folding only), and so is this match.
  def self.find_by_login(username)
    SqlBuilder.new("SELECT id, username, password_hash FROM users WHERE lower(username) = lower(")
              .bind(username).<<(")").records(self).first
  end
end
