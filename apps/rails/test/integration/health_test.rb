require "test_helper"

class HealthTest < ApiTest
  test "reports ok with the node and the version" do
    api_get "/health"
    assert_response :ok
    assert_equal({ "status" => "ok", "node_id" => "local", "version" => "0.1.0" }, json)
  end

  test "answers 503 when the database does not" do
    stub_method(ApplicationRecord.lease_connection, :select_value, ->(*) { raise PG::ConnectionBad, "gone" }) do
      api_get "/health"
    end
    assert_response :service_unavailable
    assert_equal "unavailable", json["status"]
  end

  test "sets the security headers and an X-Request-Id" do
    api_get "/health"
    assert_equal "nosniff", response.headers["X-Content-Type-Options"]
    assert_equal "DENY", response.headers["X-Frame-Options"]
    assert_equal "same-origin", response.headers["Referrer-Policy"]
    assert response.headers["X-Request-Id"].present?
  end
end
