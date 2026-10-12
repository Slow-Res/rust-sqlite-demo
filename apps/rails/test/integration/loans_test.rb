require "test_helper"

class LoansTest < ApiTest
  setup do
    sign_in
    @book = create_book(total_copies: 2)
    @member = create_member
  end

  def borrow(book = @book, member = @member) = api_post("/loans", { book_id: book.id, member_id: member.id })

  test "every endpoint needs a session" do
    cookies.delete("library_session")
    api_get "/loans"
    assert_error :unauthorized, "unauthorized"
    borrow
    assert_error :unauthorized, "unauthorized"
    api_post "/loans/1/return"
    assert_error :unauthorized, "unauthorized"
  end

  test "borrowing answers 201 with a loan due in 14 days" do
    freeze_time do
      borrow
      assert_response :created
      assert_equal %w[id book_id book_title member_id member_name borrowed_at due_at returned_at is_overdue], json.keys
      assert_equal [@book.id, "Dune", @member.id, "Grace Hopper"],
                   json.values_at("book_id", "book_title", "member_id", "member_name")
      assert_equal Clock.iso(Clock.now), json["borrowed_at"]
      assert_equal Clock.iso(Clock.now + 1_209_600), json["due_at"]
      assert_nil json["returned_at"]
      assert_equal false, json["is_overdue"]
    end
  end

  test "ids below 1 are a 422, both reported" do
    api_post "/loans", { book_id: 0, member_id: -1 }
    assert_error :unprocessable_entity, "validation_error"
    assert_equal({ "book_id" => "must be a positive id", "member_id" => "must be a positive id" }, json["error"]["fields"])
    api_post "/loans", { book_id: "1", member_id: 1 }
    assert_error :bad_request, "bad_request"
    api_post "/loans", { book_id: 1 }
    assert_error :bad_request, "bad_request"
  end

  test "the checks run in order: book, member, copies, limit" do
    api_post "/loans", { book_id: 999_999, member_id: 999_999 }
    assert_error :not_found, "not_found", "book not found"
    api_post "/loans", { book_id: @book.id, member_id: 999_999 }
    assert_error :not_found, "not_found", "member not found"

    2.times { borrow }
    assert_response :created
    borrow
    assert_error :conflict, "no_copies_available", "no copies of this book are available"
  end

  test "a member holds at most 5 loans" do
    big = create_book(title: "Big", total_copies: 10)
    5.times { borrow(big) }
    assert_response :created
    borrow(big)
    assert_error :conflict, "loan_limit_reached", "member already has the maximum of 5 active loans"
    # ... which is checked after the copies: a full book says so first
    create_loan(create_book(title: "Full", total_copies: 1), create_member(email: "x@example.com"))
    api_post "/loans", { book_id: Book.find_by(title: "Full").id, member_id: @member.id }
    assert_error :conflict, "no_copies_available"
  end

  test "return sets returned_at and frees the copy" do
    borrow
    loan_id = json["id"]
    api_post "/loans/#{loan_id}/return"
    assert_response :ok
    assert json["returned_at"]
    assert_equal false, json["is_overdue"]
    api_get "/books/#{@book.id}"
    assert_equal 2, json["available_copies"]
    api_post "/loans/#{loan_id}/return"
    assert_error :conflict, "loan_already_returned", "loan has already been returned"
    api_post "/loans/999999/return"
    assert_error :not_found, "not_found", "loan not found"
    api_post "/loans/abc/return"
    assert_error :bad_request, "bad_request"
  end

  test "list filters by status and orders by due_at, returned by returned_at descending" do
    now = Clock.now
    other = create_book(title: "Other")
    late = create_loan(@book, @member, borrowed_at: now - 40 * 86_400, due_at: now - 10)
    soon = create_loan(other, @member, due_at: now + 100)
    done1 = create_loan(@book, @member, returned_at: now - 200)
    done2 = create_loan(@book, @member, returned_at: now - 100)

    api_get "/loans"
    assert_page items: 2, total: 2
    assert_equal [late.id, soon.id], json["items"].map { |l| l["id"] }
    assert_equal [true, false], json["items"].map { |l| l["is_overdue"] }
    api_get "/loans?status=overdue"
    assert_equal [late.id], json["items"].map { |l| l["id"] }
    assert_equal 1, json["total"]
    api_get "/loans?status=returned"
    assert_equal [done2.id, done1.id], json["items"].map { |l| l["id"] }
    assert_equal [false, false], json["items"].map { |l| l["is_overdue"] }
    api_get "/loans?status=all"
    assert_page items: 4, total: 4
    api_get "/loans?status=all&book_id=#{other.id}"
    assert_equal [soon.id], json["items"].map { |l| l["id"] }
    api_get "/loans?status=all&member_id=#{@member.id}&book_id=#{@book.id}"
    assert_page items: 3, total: 3
    api_get "/loans?member_id=999999"
    assert_page items: 0, total: 0
    api_get "/loans?status=nope"
    assert_error :bad_request, "bad_request"
    api_get "/loans?member_id=x"
    assert_error :bad_request, "bad_request"
  end

  test "a loan due exactly now is not overdue yet" do
    now = Clock.now
    freeze_time do
      create_loan(@book, @member, due_at: Clock.now)
      api_get "/loans?status=overdue"
      assert_page items: 0, total: 0
      api_get "/loans"
      assert_equal false, json["items"].first["is_overdue"]
    end
  end

  test "cursor paging, active and returned" do
    now = Clock.now
    5.times { |i| create_loan(@book, @member, due_at: now + i, returned_at: (i.even? ? nil : now - i)) }
    [["active", 3], ["returned", 2]].each do |status, count|
      ids = []
      cursor = nil
      loop do
        api_get "/loans?status=#{status}&per_page=2#{"&after=#{cursor}" if cursor}"
        assert_equal count, json["total"]
        ids += json["items"].map { |l| l["id"] }
        cursor = json["next_cursor"] or break
      end
      assert_equal count, ids.size
      assert_equal ids.uniq, ids
    end
    api_get "/loans?per_page=1"
    key, id = JSON.parse(Base64.urlsafe_decode64(json["next_cursor"]))
    assert_kind_of Integer, key
    assert_equal json["items"].first["id"], id
    api_get "/loans?after=#{Pagination.encode_cursor("title", 1)}"
    assert_error :bad_request, "bad_request"
  end
end
