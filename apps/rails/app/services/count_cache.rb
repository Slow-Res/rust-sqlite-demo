# A short-lived cache for the `total` of the unfiltered list pages (and of the
# book list filtered by availability alone). The same rules as the Rust app's
# count_cache.rs: only totals are cached, a slot is reused for LIST_COUNT_TTL_MS
# (default 1000), TTL 0 caches nothing, a failed count is not cached, and the
# slot's lock is held while counting, so when a slot expires under load one
# request counts and the others wait for its answer.
#
# One cache per Puma worker (it lives in process memory), so with 2 workers the
# counts refresh about twice as often as in the Rust app.
class CountCache
  KEYS = %i[books books_available books_on_loan members loans_active loans_returned loans_all].freeze

  Slot = Struct.new(:mutex, :taken_at, :total)

  def self.shared
    @shared ||= new(Settings::LIST_COUNT_TTL_MS / 1000.0)
  end

  # `ttl` in seconds; zero disables the cache.
  def initialize(ttl)
    @ttl = ttl
    @slots = KEYS.to_h { |key| [key, Slot.new(Mutex.new, nil, nil)] }
  end

  # The cached total for `key` if it is younger than the TTL, otherwise what the
  # block returns, which is then cached. A nil key (a filtered list) just counts.
  def get_or_count(key)
    return yield if key.nil? || @ttl.zero?

    slot = @slots.fetch(key)
    slot.mutex.synchronize do
      now = monotonic
      return slot.total if slot.taken_at && now - slot.taken_at < @ttl

      total = yield
      # Stamped when the count was requested: a count that ran long must not look
      # fresher than it is.
      slot.taken_at = now
      slot.total = total
    end
  end

  private

  def monotonic = Process.clock_gettime(Process::CLOCK_MONOTONIC)
end
