# Rack middleware: a request body over 64 KiB is answered with 413 and the JSON
# error shape before routing, like the Rust app's RequestBodyLimitLayer. Only
# the declared Content-Length is checked (a chunked body is not).
class BodyLimit
  LIMIT_BYTES = 64 * 1024
  BODY = '{"error":{"code":"payload_too_large","message":"request body too large"}}'.freeze

  def initialize(app)
    @app = app
  end

  def call(env)
    return @app.call(env) unless env["CONTENT_LENGTH"].to_i > LIMIT_BYTES

    [413, { "content-type" => "application/json", "content-length" => BODY.bytesize.to_s }, [BODY]]
  end
end
