require "test_helper"

class PaginationTest < ActiveSupport::TestCase
  test "defaults and clamping" do
    params, cursor = Pagination.params(nil, nil, nil, :string)
    assert_equal [1, 20, 0, 21, nil], [params.page, params.per_page, params.offset, params.limit, cursor]
    params, = Pagination.params(0, 0, nil, :string)
    assert_equal [1, 1], [params.page, params.per_page]
    assert_equal 100, Pagination.params(nil, 500, nil, :string).first.per_page
  end

  test "offset math does not overflow" do
    params, = Pagination.params(3, 25, nil, :string)
    assert_equal [26, 50], [params.limit, params.offset]
    assert_equal (2**32 - 2) * 100, Pagination.params(2**32 - 1, 100, nil, :string).first.offset
  end

  test "a cursor makes the offset 0 and the page ignored" do
    cursor = Pagination.encode_cursor("Dune", 7)
    params, decoded = Pagination.params(5, 10, cursor, :string)
    assert_equal [0, ["Dune", 7]], [params.offset, decoded]
  end

  test "cursors round-trip" do
    assert_equal ["Dune é", 7], Pagination.decode_cursor(Pagination.encode_cursor("Dune é", 7), :string)
    assert_equal [1_700_000_000, 9], Pagination.decode_cursor(Pagination.encode_cursor(1_700_000_000, 9), :integer)
    assert_equal '["a",1]', Base64.urlsafe_decode64(Pagination.encode_cursor("a", 1))
  end

  test "malformed cursors are bad requests" do
    ["", "!!!", "e30", "WyJhIl0", "WyJhIiwiYiJd", "WyJhIiwxXQ==", "WyJhIiwxLjVd", Pagination.encode_cursor(1, 1)].each do |bad|
      error = assert_raises(ApiError, bad) { Pagination.decode_cursor(bad, :string) }
      assert_equal [400, "invalid cursor"], [error.status, error.message]
    end
    assert_raises(ApiError) { Pagination.decode_cursor(Pagination.encode_cursor("x", 1), :integer) }
    assert_raises(ApiError) { Pagination.decode_cursor(Pagination.encode_cursor(2**63, 1), :integer) }
  end

  test "search term" do
    assert_nil Pagination.search_term(nil)
    assert_nil Pagination.search_term(" \u00a0 ")
    assert_equal "a b", Pagination.search_term("  a b\u00a0")
    assert_equal 200, Pagination.search_term("a" * 200).length
    assert_equal 422, assert_raises(ApiError) { Pagination.search_term("a" * 201) }.status
  end

  test "search words, ISBN prefix, LIKE escaping" do
    assert_equal %w[J R R Tolkien], Text.search_words("J.R.R. Tolkien")
    assert_equal %w[a b c d e], Text.search_words("a_b \"c\" d*:(-e")
    assert_equal %w[Café Müller 12], Text.search_words("Café Müller 12")
    assert_empty Text.search_words("%%% --- \"")
    assert_equal "9780261102217", Text.isbn_prefix("978-0-261 10221-7")
    assert_equal "0441172", Text.isbn_prefix("0441 172")
    assert_nil Text.isbn_prefix("9780261")
    assert_nil Text.isbn_prefix("- -")
    assert_nil Text.isbn_prefix("a.b c")
    assert_equal "100\\%\\_done\\\\", Text.escape_like("100%_done\\")
  end
end
