require "test_helper"

class ErrorsTest < ApiTest
  test "an unknown /api path is a JSON 404" do
    api_get "/nope"
    assert_error :not_found, "not_found", "route not found"
    post "/api/books/1/extra"
    assert_error :not_found, "not_found"
  end

  test "a known path with another method is a JSON 405" do
    api_post "/health"
    assert_error :method_not_allowed, "method_not_allowed", "method not allowed"
    api_put "/books"
    assert_error :method_not_allowed, "method_not_allowed"
    patch "/api/books/1"
    assert_error :method_not_allowed, "method_not_allowed"
    api_get "/loans/1/return"
    assert_error :method_not_allowed, "method_not_allowed"
  end

  test "a body that is not JSON is a 400" do
    sign_in
    api_post "/books", "{not json"
    assert_error :bad_request, "bad_request"
  end

  test "a body without the JSON content type is a 400" do
    sign_in
    post "/api/books", params: { title: "x" }
    assert_error :bad_request, "bad_request"
  end

  test "a body of the wrong shape is a 400" do
    sign_in
    api_post "/books", []
    assert_error :bad_request, "bad_request"
    api_post "/books", { title: 1, author: "a", total_copies: 1 }
    assert_error :bad_request, "bad_request"
    api_post "/books", { title: "a", author: "a", total_copies: "1" }
    assert_error :bad_request, "bad_request"
    api_post "/books", { title: "a", author: "a", total_copies: 1.0 }
    assert_error :bad_request, "bad_request"
    api_post "/books", { title: "a", author: "a", total_copies: 1, published_year: 2**40 }
    assert_error :bad_request, "bad_request"
  end

  test "a body over 64 KiB is a 413" do
    sign_in
    api_post "/books", { title: "x" * 70_000, author: "a", total_copies: 1 }
    assert_error 413, "payload_too_large", "request body too large"
  end

  test "an id that is not a number is a 400, a missing one a 404" do
    api_get "/books/abc"
    assert_error :bad_request, "bad_request"
    api_get "/books/99999999999999999999"
    assert_error :bad_request, "bad_request"
    api_get "/books/1.5"
    assert_error :bad_request, "bad_request"
    api_get "/books/-4"
    assert_error :not_found, "not_found", "book not found"
  end

  test "an unexpected error is a 500 that does not leak its message" do
    stub_method(Book, :find_with_availability, ->(*) { raise "secret db detail" }) do
      api_get "/books/1"
    end
    assert_error :internal_server_error, "internal", "Internal server error"
    assert_not_includes response.body, "secret"
  end
end
