ENV["RAILS_ENV"] = "test"
# Exact totals: no caching, unless a test builds a CountCache itself.
ENV["LIST_COUNT_TTL_MS"] = "0"

require_relative "../config/environment"
require "rails/test_help"

# Request tests: the schema comes from migrations/web-postgres (loaded into the
# database DATABASE_URL names before the run, see README.md); every test runs in a
# transaction that is rolled back.
class ActiveSupport::TestCase
  # The Argon2id hash `library-web create-user` writes for PASSWORD (m=19456,t=2,p=1).
  PASSWORD = "rails-demo-pass".freeze
  PASSWORD_HASH = "$argon2id$v=19$m=19456,t=2,p=1$EZ6R4Jpve/BkgTrzEfVMWg$ztwv9awkyovV3K9ahTn6kBNWoXlHz6o3hp171vuJ4Nc".freeze

  ISBN = "978-0-13-110362-7".freeze

  # Minitest 6 has no `stub`: replace `object.name` for the duration of the block.
  def stub_method(object, name, implementation)
    singleton = object.singleton_class
    saved = :"__stubbed_#{name}"
    singleton.send(:alias_method, saved, name)
    singleton.send(:define_method, name, &implementation)
    yield
  ensure
    singleton.send(:alias_method, name, saved)
    singleton.send(:remove_method, saved)
  end

  def create_user(username = "admin", password_hash: PASSWORD_HASH)
    User.connection.insert("INSERT INTO users (username, password_hash, created_at) VALUES ('#{username}', '#{password_hash}', 0)")
  end

  def create_book(title: "Dune", author: "Frank Herbert", isbn: nil, published_year: 1965, total_copies: 3)
    now = Clock.now
    Book.create!(title: title, author: author, isbn: isbn, published_year: published_year,
                 total_copies: total_copies, created_at: now, updated_at: now)
  end

  def create_member(name: "Grace Hopper", email: "grace@example.com")
    now = Clock.now
    Member.create!(name: name, email: email, created_at: now, updated_at: now)
  end

  def create_loan(book, member, borrowed_at: Clock.now, due_at: borrowed_at + 14 * 86_400, returned_at: nil)
    Loan.create!(book_id: book.id, member_id: member.id, borrowed_at: borrowed_at, due_at: due_at,
                 returned_at: returned_at)
  end
end

class ApiTest < ActionDispatch::IntegrationTest
  def json = JSON.parse(response.body)

  def api_get(path, **options) = get("/api#{path}", **options)
  def api_post(path, body = nil, **options) = send_json(:post, path, body, options)
  def api_put(path, body = nil, **options) = send_json(:put, path, body, options)
  def api_delete(path, **options) = delete("/api#{path}", **options)

  # A session in the database and its cookie, without paying for an Argon2 hash.
  def sign_in(username = "admin")
    create_user(username)
    token = SecureRandom.hex(32)
    id = User.where(username: username).pick(:id)
    now = Clock.now
    UserSession.create!(token_hash: Authentication.hash_token(token), user_id: id, created_at: now,
                        expires_at: now + 3600)
    cookies[ApplicationController::SESSION_COOKIE] = token
    id
  end

  def assert_error(status, code, message = nil)
    assert_response status
    assert_equal code, json.dig("error", "code")
    assert_equal message, json.dig("error", "message") if message
    assert_equal %w[error], json.keys
  end

  def assert_page(items:, total:, page: 1, per_page: 20)
    assert_response :ok
    assert_equal %w[items page per_page total next_cursor], json.keys
    assert_equal items, json["items"].size
    assert_equal [page, per_page, total], json.values_at("page", "per_page", "total")
  end

  private

  def send_json(verb, path, body, options)
    body = body.to_json unless body.nil? || body.is_a?(String)
    send(verb, "/api#{path}", params: body, headers: { "CONTENT_TYPE" => "application/json" }.merge(options[:headers] || {}))
  end
end
