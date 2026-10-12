class ApplicationController < ActionController::API
  SESSION_COOKIE = "library_session".freeze
  JSON_CONTENT_TYPE = %r{\Aapplication/(?:json|.+\+json)\z}i
  UINT32_MAX = 2**32 - 1
  INT64 = (-(2**63))..(2**63 - 1)
  INT32 = (-(2**31))..(2**31 - 1)

  # The JSON body is parsed only where an action asks for it.
  wrap_parameters false

  # Later declarations win: the specific handlers come after the catch-all.
  rescue_from StandardError, with: :render_internal
  rescue_from ActionController::BadRequest, ActionDispatch::Http::Parameters::ParseError,
              with: ->(error) { render_error(ApiError.bad_request(error.cause&.message || error.message)) }
  rescue_from ActiveRecord::RecordNotUnique, with: :render_conflict
  rescue_from ApiError, with: :render_error

  private

  # `Content-Type: application/json`, as the Rust app sends it (Rails adds a charset).
  def render(*)
    super.tap { response.set_header("Content-Type", "application/json") if response.media_type == "application/json" }
  end

  # --- errors ---

  def render_error(error)
    render json: error.to_body, status: error.status
  end

  # A unique index of the schema was violated; the index name says which.
  def render_conflict(error)
    case error.message
    when /uq_books_isbn/ then render_error(ApiError.conflict("isbn_taken", "a book with this ISBN already exists"))
    when /uq_members_email/ then render_error(ApiError.conflict("email_taken", "a member with this email already exists"))
    else render_internal(error)
    end
  end

  # The inner error is logged, never sent to the client.
  def render_internal(error)
    Rails.logger.error("internal error: #{error.class}: #{error.message}\n#{error.backtrace&.first(10)&.join("\n")}")
    render_error(ApiError.internal)
  end

  # --- authentication ---

  # Add `before_action :require_user!` to protect an action: the request is
  # rejected with 401 before the rest of the input is read.
  def require_user!
    token = request.cookies[SESSION_COOKIE]
    @current_user = token && Authentication.user_for(token)
    raise ApiError.unauthorized unless @current_user
  end

  # --- request input ---
  # Types are checked strictly (a string where a number is expected is a 400, as
  # with serde); validation of the values is up to the models.

  # The JSON object of the request body.
  def json_body
    raise ApiError.bad_request("expected request with `Content-Type: application/json`") unless request.media_type&.match?(JSON_CONTENT_TYPE)

    request.request_parameters
  end

  def body_string(body, key, optional: false)
    value = body[key]
    return nil if value.nil? && optional
    return value if value.is_a?(String)

    raise ApiError.bad_request("invalid request body: `#{key}` must be a string")
  end

  def body_integer(body, key, range, optional: false)
    value = body[key]
    return nil if value.nil? && optional
    return value if value.is_a?(Integer) && range.cover?(value)

    raise ApiError.bad_request("invalid request body: `#{key}` must be an integer in #{range}")
  end

  def query_string(name)
    value = request.query_parameters[name]
    value if value.is_a?(String)
  end

  def query_uint32(name)
    value = query_string(name)
    return nil unless value
    return value.to_i if value.match?(/\A\+?[0-9]+\z/) && value.to_i <= UINT32_MAX

    raise ApiError.bad_request("invalid query string: `#{name}` must be an integer from 0 to #{UINT32_MAX}")
  end

  def query_int64(name)
    value = query_string(name)
    return nil unless value
    return value.to_i if value.match?(/\A[+-]?[0-9]+\z/) && INT64.cover?(value.to_i)

    raise ApiError.bad_request("invalid query string: `#{name}` must be an integer")
  end

  def query_enum(name, allowed)
    value = query_string(name)
    return nil unless value
    return value if allowed.include?(value)

    raise ApiError.bad_request("invalid query string: `#{name}` must be one of #{allowed.join(", ")}")
  end

  # The `{id}` of the path.
  def path_id
    value = request.path_parameters[:id]
    return value.to_i if value.match?(/\A[+-]?[0-9]+\z/) && INT64.cover?(value.to_i)

    raise ApiError.bad_request("invalid path parameter: `id` must be an integer")
  end
end
