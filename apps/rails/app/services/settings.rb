# Environment settings (the ones the Rust app reads, same names and defaults).
# A bad value stops the boot.
module Settings
  MAX_SESSION_TTL_HOURS = 87_600 # ten years

  def self.boolean(name, default)
    case ENV.fetch(name, default.to_s)
    when "true" then true
    when "false" then false
    else raise "#{name} must be `true` or `false`"
    end
  end

  def self.session_ttl_hours
    hours = Integer(ENV.fetch("SESSION_TTL_HOURS", 168))
    raise "SESSION_TTL_HOURS must be between 1 and #{MAX_SESSION_TTL_HOURS}, got #{hours}" unless (1..MAX_SESSION_TTL_HOURS).cover?(hours)

    hours
  end

  COOKIE_SECURE = boolean("COOKIE_SECURE", false)
  SESSION_TTL_SECONDS = session_ttl_hours * 3600
  NODE_ID = ENV.fetch("NODE_ID", "local")
  LIST_COUNT_TTL_MS = Integer(ENV.fetch("LIST_COUNT_TTL_MS", 1000))
end
