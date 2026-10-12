require_relative "boot"

# Only the frameworks the API uses: no Action Cable, Active Storage, Active Job,
# Action Mailer or asset pipeline.
require "rails"
require "active_record/railtie"
require "action_controller/railtie"

Bundler.require(*Rails.groups)

require_relative "../lib/body_limit"
require_relative "../lib/trailing_slash"

module LibraryRails
  # The version of the Rust app this one mirrors (workspace version); /api/health reports it.
  VERSION = "0.1.0"

  class Application < Rails::Application
    config.load_defaults 8.1
    config.api_only = true
    config.time_zone = "UTC"

    # The schema belongs to migrations/web-postgres (plain SQL, run by psql or the
    # Rust app). Rails runs no migrations, dumps no schema.rb and checks nothing.
    config.active_record.dump_schema_after_migration = false
    config.active_record.maintain_test_schema = false

    config.filter_parameters += [:password]

    # Same three security headers as the Rust app's layers.
    config.action_dispatch.default_headers = {
      "X-Content-Type-Options" => "nosniff",
      "X-Frame-Options" => "DENY",
      "Referrer-Policy" => "same-origin"
    }

    # Requests over 64 KiB get the JSON 413 before anything else runs; responses
    # are gzipped when the client asks (tower-http's CompressionLayer does both).
    config.middleware.insert_before 0, BodyLimit
    config.middleware.insert_after BodyLimit, TrailingSlash
    config.middleware.use Rack::Deflater

    # Errors raised outside a controller (a bad query string, say) still get the
    # JSON error shape.
    config.exceptions_app = ->(env) { ErrorsController.action(:exception).call(env) }
  end
end
