# An error the API answers with, in the shape of spec 5.5:
# {"error": {"code", "message", "fields"?}}. Controllers raise it;
# ApplicationController renders it. Anything else that escapes is a 500.
class ApiError < StandardError
  attr_reader :status, :code, :fields

  def initialize(status, code, message, fields: nil)
    super(message)
    @status = status
    @code = code
    @fields = fields
  end

  # `fields` maps a field name to its message. They are listed in name order.
  def self.validation(fields)
    new(422, "validation_error", "Some fields are invalid", fields: fields.sort.to_h.presence)
  end

  def self.bad_request(message) = new(400, "bad_request", message)
  def self.unauthorized = new(401, "unauthorized", "Authentication required")
  def self.invalid_credentials = new(401, "unauthorized", "invalid username or password")
  def self.not_found(what) = new(404, "not_found", "#{what} not found")
  def self.method_not_allowed = new(405, "method_not_allowed", "method not allowed")
  def self.conflict(code, message) = new(409, code, message)
  def self.internal = new(500, "internal", "Internal server error")

  def to_body
    error = { code: code, message: message }
    error[:fields] = fields if fields
    { error: error }
  end
end
