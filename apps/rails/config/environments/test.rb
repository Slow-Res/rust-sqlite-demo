Rails.application.configure do
  config.enable_reloading = false
  config.eager_load = false
  config.consider_all_requests_local = true
  config.cache_store = :null_store
  config.action_dispatch.show_exceptions = :rescuable
  config.active_support.deprecation = :stderr
  config.logger = ActiveSupport::Logger.new(ENV["TEST_LOG"] ? $stderr : File::NULL) # TEST_LOG=1 shows errors
  config.secret_key_base = "local-only-test-secret-key-base"
end
