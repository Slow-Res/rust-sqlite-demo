require "test_helper"

class MembersTest < ApiTest
  def member_body(**overrides) = { name: "Grace Hopper", email: "grace@example.com" }.merge(overrides)

  test "every endpoint needs a session" do
    member = create_member
    api_get "/members"
    assert_error :unauthorized, "unauthorized"
    api_get "/members/#{member.id}"
    assert_error :unauthorized, "unauthorized"
    api_post "/members", member_body
    assert_error :unauthorized, "unauthorized"
    api_put "/members/#{member.id}", member_body
    assert_error :unauthorized, "unauthorized"
    api_delete "/members/#{member.id}"
    assert_error :unauthorized, "unauthorized"
  end

  test "create trims the name, lowercases the email, answers 201" do
    sign_in
    api_post "/members", member_body(name: "  Ada ", email: " Ada@Example.COM ")
    assert_response :created
    assert_equal %w[id name email active_loans created_at updated_at], json.keys
    assert_equal ["Ada", "ada@example.com", 0], json.values_at("name", "email", "active_loans")
  end

  test "validation checks both fields" do
    sign_in
    api_post "/members", member_body(name: " ", email: "nope")
    assert_error :unprocessable_entity, "validation_error"
    assert_equal({ "email" => "must be a valid email address", "name" => "must not be empty" }, json["error"]["fields"])
  end

  test "the email rules" do
    sign_in
    ["", "a", "a@", "@b.c", "a@b", "a@.b.c", "a@b.c.", "a@b@c.d"].each do |email|
      api_post "/members", member_body(email: email)
      assert_equal({ "email" => "must be a valid email address" }, json["error"]["fields"], email.inspect)
    end
    api_post "/members", member_body(email: "#{"a" * 250}@b.cd")
    assert_equal({ "email" => "must be at most 254 characters" }, json["error"]["fields"])
    api_post "/members", member_body(name: "n" * 201)
    assert_equal({ "name" => "must be at most 200 characters" }, json["error"]["fields"])
  end

  test "a duplicate email is a 409 whatever its case" do
    sign_in
    create_member(email: "grace@example.com")
    api_post "/members", member_body(email: "GRACE@example.com")
    assert_error :conflict, "email_taken", "a member with this email already exists"
  end

  test "show counts active loans, 404 for an unknown member" do
    sign_in
    member = create_member
    create_loan(create_book, member)
    create_loan(create_book(title: "B"), member, returned_at: Clock.now)
    api_get "/members/#{member.id}"
    assert_response :ok
    assert_equal 1, json["active_loans"]
    api_get "/members/0"
    assert_error :not_found, "not_found", "member not found"
  end

  test "update replaces name and email, 422 before 404, 409 for another's email" do
    sign_in
    member = create_member
    other = create_member(name: "Other", email: "other@example.com")
    api_put "/members/#{member.id}", member_body(name: "Grace B", email: "g@example.com")
    assert_response :ok
    assert_equal ["Grace B", "g@example.com"], json.values_at("name", "email")
    api_put "/members/#{member.id}", member_body(name: "Same", email: "g@example.com")
    assert_response :ok
    api_put "/members/999999", member_body(name: "")
    assert_error :unprocessable_entity, "validation_error"
    api_put "/members/999999", member_body
    assert_error :not_found, "not_found", "member not found"
    # Last: the unique violation aborts the transaction this test runs in.
    api_put "/members/#{member.id}", member_body(email: other.email)
    assert_error :conflict, "email_taken"
  end

  test "delete removes the member and the returned loans, refuses active ones" do
    sign_in
    member = create_member
    create_loan(create_book, member, returned_at: Clock.now)
    api_delete "/members/#{member.id}"
    assert_response :no_content
    assert_equal 0, Loan.count
    api_delete "/members/#{member.id}"
    assert_error :not_found, "not_found"

    busy = create_member(email: "busy@example.com")
    create_loan(create_book(title: "B"), busy)
    api_delete "/members/#{busy.id}"
    assert_error :conflict, "member_has_active_loans", "member has active loans and cannot be deleted"
  end

  test "list: name order, paging, cursor, substring search with literal wildcards" do
    sign_in
    ["Bob", "alice", "Carol_X", "100% Dave"].each_with_index do |name, i|
      create_member(name: name, email: "m#{i}@example.com")
    end
    api_get "/members"
    assert_page items: 4, total: 4
    assert_equal ["100% Dave", "Bob", "Carol_X", "alice"], json["items"].map { |m| m["name"] }

    api_get "/members?per_page=3"
    assert_equal ["Carol_X", Member.find_by(name: "Carol_X").id], JSON.parse(Base64.urlsafe_decode64(json["next_cursor"]))
    api_get "/members?per_page=3&after=#{json["next_cursor"]}"
    assert_page items: 1, total: 4, per_page: 3
    assert_nil json["next_cursor"]

    { "ALICE" => 1, "m2@" => 1, "%" => 1, "_" => 1, "carol" => 1, "zzz" => 0, "  " => 4 }.each do |q, matches|
      api_get "/members", params: { q: q }
      assert_page items: matches, total: matches
    end
    api_get "/members?after=#{Pagination.encode_cursor(5, 1)}"
    assert_error :bad_request, "bad_request"
  end
end
