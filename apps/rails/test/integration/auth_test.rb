require "test_helper"

class AuthTest < ApiTest
  test "login accepts a hash made by the Rust app and sets the session cookie" do
    id = create_user
    api_post "/auth/login", { username: "admin", password: PASSWORD }
    assert_response :ok
    assert_equal({ "id" => id, "username" => "admin" }, json)

    cookie = response.headers["Set-Cookie"]
    assert_match(/\Alibrary_session=[0-9a-f]{64};/, cookie)
    assert_match(/max-age=604800/i, cookie)
    assert_match(/path=\//i, cookie)
    assert_match(/httponly/i, cookie)
    assert_match(/samesite=lax/i, cookie)
    assert_no_match(/secure/i, cookie)

    api_get "/auth/me"
    assert_response :ok
    assert_equal({ "id" => id, "username" => "admin" }, json)
  end

  test "the session is stored as the SHA-256 of the token" do
    create_user
    api_post "/auth/login", { username: "admin", password: PASSWORD }
    token = response.headers["Set-Cookie"][/library_session=(\h+)/, 1]
    session = UserSession.find(Authentication.hash_token(token))
    assert_in_delta Clock.now + 168 * 3600, session.expires_at, 5
  end

  test "the username is trimmed and matched without regard to ASCII case" do
    create_user("Admin")
    api_post "/auth/login", { username: "  aDMIN ", password: PASSWORD }
    assert_response :ok
    assert_equal "Admin", json["username"]
  end

  test "a wrong password and an unknown user get the same 401" do
    create_user
    api_post "/auth/login", { username: "admin", password: "wrong password" }
    assert_error :unauthorized, "unauthorized", "invalid username or password"
    assert_nil response.headers["Set-Cookie"]
    api_post "/auth/login", { username: "nobody", password: PASSWORD }
    assert_error :unauthorized, "unauthorized", "invalid username or password"
  end

  test "an unknown user still costs one hash verification" do
    calls = []
    stub_method(PasswordHasher, :verify, ->(_password, hash) { calls << hash; false }) do
      api_post "/auth/login", { username: "nobody", password: "whatever" }
    end
    assert_equal [PasswordHasher::DUMMY_HASH], calls
  end

  test "blank fields are a 422 with field errors" do
    api_post "/auth/login", { username: "  ", password: "" }
    assert_error :unprocessable_entity, "validation_error", "Some fields are invalid"
    assert_equal({ "password" => "must not be empty", "username" => "must not be empty" }, json["error"]["fields"])
  end

  test "a missing or mistyped field is a 400" do
    api_post "/auth/login", { username: "admin" }
    assert_error :bad_request, "bad_request"
    api_post "/auth/login", { username: 1, password: "x" }
    assert_error :bad_request, "bad_request"
  end

  test "logging in sweeps the expired sessions" do
    user_id = create_user
    UserSession.create!(token_hash: "expired", user_id: user_id, created_at: 0, expires_at: Clock.now - 1)
    UserSession.create!(token_hash: "alive", user_id: user_id, created_at: 0, expires_at: Clock.now + 100)
    api_post "/auth/login", { username: "admin", password: PASSWORD }
    assert_response :ok
    assert_not UserSession.exists?("expired")
    assert UserSession.exists?("alive")
  end

  test "me needs a valid, unexpired session" do
    api_get "/auth/me"
    assert_error :unauthorized, "unauthorized", "Authentication required"

    user_id = create_user
    cookies["library_session"] = "deadbeef"
    api_get "/auth/me"
    assert_error :unauthorized, "unauthorized"

    UserSession.create!(token_hash: Authentication.hash_token("old"), user_id: user_id, created_at: 0,
                        expires_at: Clock.now)
    cookies["library_session"] = "old"
    api_get "/auth/me"
    assert_error :unauthorized, "unauthorized"
  end

  test "logout deletes the session and clears the cookie" do
    sign_in
    api_post "/auth/logout"
    assert_response :no_content
    assert_empty response.body
    assert_match(/library_session=;.*max-age=0/i, response.headers["Set-Cookie"])
    assert_equal 0, UserSession.count

    api_post "/auth/logout", nil, headers: { "Cookie" => "library_session=gone" }
    assert_error :unauthorized, "unauthorized"
  end

  test "login and logout need a JSON body or a session" do
    api_post "/auth/logout"
    assert_error :unauthorized, "unauthorized"
    post "/api/auth/login"
    assert_error :bad_request, "bad_request"
  end
end
