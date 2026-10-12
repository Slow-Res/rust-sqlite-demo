# The current time as Unix seconds, which is what every timestamp column holds.
# Tests move it with `travel_to`.
module Clock
  def self.now = Time.now.to_i

  def self.year = Time.at(now).utc.year

  # Timestamps leave the API as RFC 3339 in UTC, whole seconds: 2026-10-13T12:34:56Z.
  def self.iso(seconds) = seconds && Time.at(seconds).utc.iso8601
end
