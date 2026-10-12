# Rack middleware: `/api/books/` is an unknown route, not `/api/books` (Rails
# would ignore the trailing slash; axum does not).
class TrailingSlash
  BODY = '{"error":{"code":"not_found","message":"route not found"}}'.freeze

  def initialize(app)
    @app = app
  end

  def call(env)
    path = env["PATH_INFO"]
    return @app.call(env) unless path.start_with?("/api") && path.length > 1 && path.end_with?("/")

    [404, { "content-type" => "application/json", "content-length" => BODY.bytesize.to_s }, [BODY]]
  end
end
