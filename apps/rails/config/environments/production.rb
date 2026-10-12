Rails.application.configure do
  config.enable_reloading = false
  config.eager_load = true
  config.consider_all_requests_local = false

  # LOCAL ONLY: a fixed default so the load test needs no secret. The app signs
  # and encrypts nothing (the session token is random and stored hashed), so
  # nothing depends on it.
  config.secret_key_base = ENV.fetch("SECRET_KEY_BASE", "local-only-load-test-secret-key-base-0123456789abcdef0123456789abcdef")

  # Warn by default: no per-request lines, like the Rust app. RAILS_LOG_LEVEL=info
  # brings them back.
  config.logger = ActiveSupport::Logger.new($stdout)
  config.log_level = ENV.fetch("RAILS_LOG_LEVEL", "warn").to_sym
  config.log_tags = [:request_id]

  # TLS ends at the proxy, if anywhere; COOKIE_SECURE decides the cookie flag.
  config.force_ssl = false

  # The totals cache is CountCache; there is nothing else to cache.
  config.cache_store = :null_store

  config.active_support.report_deprecations = false
end
