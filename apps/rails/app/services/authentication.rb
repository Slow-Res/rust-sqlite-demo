# Librarian sessions: login, logout and the lookup of a session cookie (spec 5.7).
module Authentication
  TOKEN_BYTES = 32

  Login = Data.define(:id, :username, :token)

  # Verify the credentials and open a session. `username` is already trimmed.
  # Raises the 401 of a failed login: the same for an unknown user and a wrong
  # password.
  def self.login(username, password)
    user = User.find_by_login(username)
    # Always run one verification, even for an unknown user, so both failure paths
    # take about as long.
    matches = PasswordHasher.verify(password, user ? user.password_hash : PasswordHasher::DUMMY_HASH)
    raise ApiError.invalid_credentials unless user && matches

    now = Clock.now
    token = SecureRandom.hex(TOKEN_BYTES)
    UserSession.create!(token_hash: hash_token(token), user_id: user.id, created_at: now,
                        expires_at: now + Settings::SESSION_TTL_SECONDS)
    # Housekeeping: expired sessions are swept whenever someone logs in. The session
    # already exists, so a failed sweep must not fail the login.
    begin
      UserSession.delete_expired(now)
    rescue StandardError => e
      Rails.logger.warn("failed to sweep expired sessions: #{e.class}: #{e.message}")
    end

    Login.new(id: user.id, username: user.username, token: token)
  end

  def self.logout(token)
    UserSession.where(token_hash: hash_token(token)).delete_all
  end

  # `[id, username]` of the user of the raw session token, or nil if it is unknown or expired.
  def self.user_for(token)
    UserSession.user_for(hash_token(token), Clock.now)
  end

  # What the database stores instead of the token: its SHA-256, hex-encoded. If the
  # database leaks, the hashes cannot be replayed as cookies.
  def self.hash_token(token) = Digest::SHA256.hexdigest(token)
end
