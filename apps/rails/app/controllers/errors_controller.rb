# The JSON answers for routes that do not exist, wrong methods, and errors raised
# outside a controller (the exceptions app).
class ErrorsController < ApplicationController
  CODES = { 400 => "bad_request", 404 => "not_found", 405 => "method_not_allowed",
            408 => "request_timeout", 413 => "payload_too_large" }.freeze

  def not_found
    raise ApiError.not_found("route")
  end

  def method_not_allowed
    response.set_header("Allow", request.path_parameters[:allow])
    raise ApiError.method_not_allowed
  end

  # Called by ActionDispatch::ShowExceptions with the exception in the env.
  def exception
    error = request.get_header("action_dispatch.exception")
    status = ActionDispatch::ExceptionWrapper.status_code_for_exception(error.class.name)
    render_error(ApiError.new(status, CODES.fetch(status, "internal"),
                              status == 500 ? "Internal server error" : Rack::Utils::HTTP_STATUS_CODES[status].to_s.downcase))
  end
end
