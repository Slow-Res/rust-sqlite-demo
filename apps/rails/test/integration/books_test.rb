require "test_helper"

class BooksTest < ApiTest
  def book_body(**overrides)
    { title: "Dune", author: "Frank Herbert", isbn: nil, published_year: 1965, total_copies: 3 }.merge(overrides)
  end

  test "create needs a session" do
    api_post "/books", book_body
    assert_error :unauthorized, "unauthorized", "Authentication required"
  end

  test "create answers 201 with the normalized book" do
    sign_in
    api_post "/books", book_body(title: "  Dune ", isbn: " #{ISBN} ")
    assert_response :created
    book = json
    assert_equal %w[id title author isbn published_year total_copies available_copies created_at updated_at], book.keys
    assert_equal "Dune", book["title"]
    assert_equal "9780131103627", book["isbn"]
    assert_equal 3, book["available_copies"]
    assert_match(/\A\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ\z/, book["created_at"])
    assert_equal book["created_at"], book["updated_at"]
  end

  test "isbn and published_year may be absent, and a blank isbn is none" do
    sign_in
    api_post "/books", { title: "A", author: "B", total_copies: 1 }
    assert_response :created
    assert_nil json["isbn"]
    assert_nil json["published_year"]
    api_post "/books", book_body(isbn: "   ")
    assert_response :created
    assert_nil json["isbn"]
  end

  test "ISBN-10 with an X check digit is accepted and uppercased" do
    sign_in
    api_post "/books", book_body(isbn: "0-8044-2957-x")
    assert_response :created
    assert_equal "080442957X", json["isbn"]
  end

  test "validation reports every bad field, in name order" do
    sign_in
    api_post "/books", { title: " ", author: "", isbn: "nope", published_year: 0, total_copies: 0 }
    assert_error :unprocessable_entity, "validation_error", "Some fields are invalid"
    assert_equal({ "author" => "must not be empty", "isbn" => "must be a valid ISBN-10 or ISBN-13",
                   "published_year" => "must be a valid year", "title" => "must not be empty",
                   "total_copies" => "must be between 1 and 1000" }, json["error"]["fields"])
    assert_equal json["error"]["fields"].keys, json["error"]["fields"].keys.sort
  end

  test "length limits count characters after trimming" do
    sign_in
    api_post "/books", book_body(title: " #{"é" * 300} ", author: "a" * 200)
    assert_response :created
    api_post "/books", book_body(title: "é" * 301, author: "a" * 201)
    assert_equal({ "author" => "must be at most 200 characters", "title" => "must be at most 300 characters" },
                 json["error"]["fields"])
  end

  test "year bounds are one to next year, copies one to a thousand" do
    sign_in
    api_post "/books", book_body(published_year: Clock.year + 1, total_copies: 1000)
    assert_response :created
    api_post "/books", book_body(published_year: Clock.year + 2, total_copies: 1001)
    assert_equal %w[published_year total_copies], json["error"]["fields"].keys
  end

  test "a duplicate ISBN is a 409, in any spelling" do
    sign_in
    create_book(isbn: "9780131103627")
    api_post "/books", book_body(isbn: ISBN)
    assert_error :conflict, "isbn_taken", "a book with this ISBN already exists"
  end

  test "show is public and 404s" do
    book = create_book
    api_get "/books/#{book.id}"
    assert_response :ok
    assert_equal "Dune", json["title"]
    api_get "/books/0"
    assert_error :not_found, "not_found", "book not found"
  end

  test "available_copies counts the unreturned loans only" do
    book = create_book(total_copies: 3)
    member = create_member
    create_loan(book, member)
    create_loan(book, member, returned_at: Clock.now)
    api_get "/books/#{book.id}"
    assert_equal 2, json["available_copies"]
  end

  test "update replaces every field and keeps created_at" do
    sign_in
    book = create_book(isbn: "9780131103627")
    api_put "/books/#{book.id}", book_body(title: "Dune Messiah", isbn: ISBN, total_copies: 5)
    assert_response :ok
    assert_equal ["Dune Messiah", "9780131103627", 5], json.values_at("title", "isbn", "total_copies")
    assert_equal Clock.iso(book.created_at), json["created_at"]
  end

  test "update checks validation before existence, and needs a session" do
    api_put "/books/1", book_body
    assert_error :unauthorized, "unauthorized"
    sign_in
    api_put "/books/999999", book_body(title: "")
    assert_error :unprocessable_entity, "validation_error"
    api_put "/books/999999", book_body
    assert_error :not_found, "not_found", "book not found"
  end

  test "update refuses fewer copies than loans, allows equal, and refuses a taken ISBN" do
    sign_in
    book = create_book(total_copies: 3)
    member = create_member
    2.times { create_loan(book, member) }
    api_put "/books/#{book.id}", book_body(total_copies: 1)
    assert_error :conflict, "copies_below_active_loans",
                 "total copies cannot be less than the copies currently on loan"
    api_put "/books/#{book.id}", book_body(total_copies: 2)
    assert_response :ok
    assert_equal 0, json["available_copies"]

    create_book(title: "Other", isbn: "9780306406157")
    api_put "/books/#{book.id}", book_body(isbn: "978-0-306-40615-7")
    assert_error :conflict, "isbn_taken"
    api_put "/books/#{book.id}", book_body(isbn: nil)
    assert_response :ok
  end

  test "a book keeps its own ISBN on update" do
    sign_in
    book = create_book(isbn: "9780131103627")
    api_put "/books/#{book.id}", book_body(isbn: ISBN)
    assert_response :ok
  end

  test "delete removes the book and its returned loans" do
    sign_in
    book = create_book
    create_loan(book, create_member, returned_at: Clock.now)
    api_delete "/books/#{book.id}"
    assert_response :no_content
    assert_empty response.body
    assert_equal 0, Loan.count
    api_delete "/books/#{book.id}"
    assert_error :not_found, "not_found", "book not found"
  end

  test "delete is refused while a loan is active, and needs a session" do
    book = create_book
    create_loan(book, create_member)
    api_delete "/books/#{book.id}"
    assert_error :unauthorized, "unauthorized"
    sign_in
    api_delete "/books/#{book.id}"
    assert_error :conflict, "book_has_active_loans", "book has active loans and cannot be deleted"
  end

  # --- the list ---

  test "list is public, ordered by title then id, with the page envelope" do
    %w[b A a].each { |title| create_book(title: title) }
    api_get "/books"
    assert_page items: 3, total: 3
    assert_equal %w[A a b], json["items"].map { |b| b["title"] }
    assert_nil json["next_cursor"]
  end

  test "an empty list" do
    api_get "/books"
    assert_page items: 0, total: 0
  end

  test "offset paging, clamping and next_cursor" do
    5.times { |i| create_book(title: "Book #{i}") }
    api_get "/books?per_page=2&page=2"
    assert_page items: 2, total: 5, page: 2, per_page: 2
    assert_equal ["Book 2", "Book 3"], json["items"].map { |b| b["title"] }
    assert json["next_cursor"]
    api_get "/books?per_page=2&page=3"
    assert_page items: 1, total: 5, page: 3, per_page: 2
    assert_nil json["next_cursor"]
    api_get "/books?per_page=5"
    assert_nil json["next_cursor"] # ends exactly at the last row
    api_get "/books?per_page=0&page=0"
    assert_page items: 1, total: 5, page: 1, per_page: 1
    api_get "/books?per_page=5000"
    assert_equal 100, json["per_page"]
    api_get "/books?page=9"
    assert_page items: 0, total: 5, page: 9
  end

  test "cursor paging walks every book once" do
    7.times { |i| create_book(title: "Book #{i}") }
    seen = []
    cursor = nil
    loop do
      api_get "/books?per_page=3#{"&after=#{cursor}&page=9" if cursor}"
      assert_response :ok
      assert_equal 9, json["page"] if cursor
      assert_equal 7, json["total"]
      seen += json["items"].map { |b| b["title"] }
      cursor = json["next_cursor"] or break
    end
    assert_equal (0..6).map { |i| "Book #{i}" }, seen
  end

  test "the cursor is base64url of [title, id]" do
    create_book(title: "Zé") # sorts after "Zz": bytewise
    create_book(title: "Zz")
    api_get "/books?per_page=1&after=#{Pagination.encode_cursor("A", 0)}"
    assert_equal ["Zz", Book.find_by(title: "Zz").id], JSON.parse(Base64.urlsafe_decode64(json["next_cursor"]))
    assert_no_match(/[=+\/]/, json["next_cursor"])
  end

  test "bad query strings and cursors are 400s" do
    ["page=abc", "page=-1", "page=", "page=4294967296", "per_page=x", "availability=some", "availability=",
     "after=", "after=!!!", "after=e30", "after=WyJhIl0", "after=#{Pagination.encode_cursor(1, 1)}"].each do |query|
      api_get "/books?#{query}"
      assert_error :bad_request, "bad_request"
    end
    api_get "/books?after=!!!"
    assert_equal "invalid cursor", json["error"]["message"]
  end

  test "a q over 200 characters is a 422 on q" do
    api_get "/books?q=#{"a" * 201}"
    assert_error :unprocessable_entity, "validation_error"
    assert_equal({ "q" => "must be at most 200 characters" }, json["error"]["fields"])
    api_get "/books?q=#{"a" * 200}"
    assert_response :ok
  end

  test "word-prefix search: every word is a prefix of some word of title, author or ISBN" do
    create_book(title: "Ancient Anchor", author: "Ada Lovelace", isbn: "9780131103627")
    create_book(title: "Ancient Archive", author: "Bob O'Brien")
    create_book(title: "Quillfeather", author: "Eve")
    {
      "anc ada" => 1, "anc" => 2, "ANC" => 2, "quill" => 1, "eather" => 0, "brien" => 1, "obrien" => 0,
      "9780131" => 1, "978-0-131" => 1, "978 0131" => 1, "  anc  " => 2, "zzz" => 0, "ada bob" => 0,
      "anc*:(&|!" => 2, "'" => 1, "" => 3, "   " => 3
    }.each do |q, matches|
      api_get "/books", params: { q: q }
      assert_page items: matches, total: matches
    end
  end

  test "a word search counts every match, also past a cursor and on an empty page" do
    5.times { |i| create_book(title: "Quill #{i}") }
    create_book(title: "Other")
    api_get "/books?q=quill&per_page=2"
    assert_page items: 2, total: 5, per_page: 2
    api_get "/books?q=quill&per_page=2&after=#{json["next_cursor"]}"
    assert_page items: 2, total: 5, per_page: 2
    api_get "/books?q=quill&per_page=2&page=9"
    assert_page items: 0, total: 5, page: 9, per_page: 2
    api_get "/books?q=nomatch"
    assert_page items: 0, total: 0
  end

  test "a q without words is a literal substring match" do
    create_book(title: "100% Pure", author: "A")
    create_book(title: "Plain", author: "B")
    create_book(title: "Hash ## Tag", author: "C")
    api_get "/books", params: { q: "%" }
    assert_page items: 1, total: 1
    api_get "/books", params: { q: "##" }
    assert_page items: 1, total: 1
    api_get "/books", params: { q: "_" }
    assert_page items: 0, total: 0
  end

  test "availability filters: a partly loaned book is both, a full one only on_loan" do
    member = create_member
    free = create_book(title: "Free", total_copies: 2)
    partly = create_book(title: "Partly", total_copies: 2)
    full = create_book(title: "Full", total_copies: 1)
    create_loan(partly, member)
    create_loan(full, member)
    create_loan(free, member, returned_at: Clock.now)

    api_get "/books?availability=available"
    assert_page items: 2, total: 2
    assert_equal %w[Free Partly], json["items"].map { |b| b["title"] }
    api_get "/books?availability=on_loan"
    assert_page items: 2, total: 2
    assert_equal %w[Full Partly], json["items"].map { |b| b["title"] }
    api_get "/books?availability=all"
    assert_page items: 3, total: 3
    api_get "/books?availability=on_loan&q=par"
    assert_page items: 1, total: 1
  end

  test "totals of unfiltered lists are cached for the TTL, items are not" do
    create_book(title: "One")
    cache = CountCache.new(60)
    stub_method(CountCache, :shared, -> { cache }) do
      api_get "/books"
      assert_equal 1, json["total"]
      create_book(title: "Two")
      api_get "/books"
      assert_equal 2, json["items"].size
      assert_equal 1, json["total"]
      api_get "/books?q=two"
      assert_equal 1, json["total"] # searches always count
    end
  end
end
