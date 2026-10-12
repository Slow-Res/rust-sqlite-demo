require "test_helper"

# Real transactions on several connections: no wrapping test transaction, so the
# tables are emptied by hand.
class ConcurrencyTest < ActiveSupport::TestCase
  self.use_transactional_tests = false

  setup { clean }
  teardown { clean }

  def clean
    [Loan, Member, Book, UserSession, User].each(&:delete_all)
  end

  def call(verb, path, body, token)
    env = Rack::MockRequest.env_for("/api#{path}", method: verb, input: body.to_json,
                                    "CONTENT_TYPE" => "application/json", "HTTP_COOKIE" => "library_session=#{token}")
    status, _headers, response_body = Rails.application.call(env)
    [status, JSON.parse(response_body.each.to_a.join)]
  ensure
    ApplicationRecord.connection_handler.clear_active_connections!
  end

  test "two simultaneous borrows of the last copy: one wins, one gets a 409" do
    user_id = create_user
    token = SecureRandom.hex(32)
    UserSession.create!(token_hash: Authentication.hash_token(token), user_id: user_id, created_at: 0,
                        expires_at: Clock.now + 3600)
    book = create_book(total_copies: 1)
    members = Array.new(8) { |i| create_member(name: "M#{i}", email: "m#{i}@example.com") }

    results = members.map do |member|
      Thread.new { call("POST", "/loans", { book_id: book.id, member_id: member.id }, token) }
    end.map(&:value)

    assert_equal [201] + [409] * 7, results.map(&:first).sort
    assert_equal ["no_copies_available"], results.select { |s, _| s == 409 }.map { |_, b| b.dig("error", "code") }.uniq
    assert_equal 1, Loan.where(returned_at: nil).count
  end
end
