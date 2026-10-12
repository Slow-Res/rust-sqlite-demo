require "test_helper"

class CountCacheTest < ActiveSupport::TestCase
  test "a hit within the TTL does not count again" do
    cache = CountCache.new(60)
    calls = 0
    3.times { assert_equal 7, cache.get_or_count(:books) { calls += 1; 7 } }
    assert_equal 1, calls
  end

  test "an expired entry is counted again" do
    cache = CountCache.new(0.05)
    totals = [1, 2].each
    assert_equal 1, cache.get_or_count(:books) { totals.next }
    sleep 0.08
    assert_equal 2, cache.get_or_count(:books) { totals.next }
  end

  test "TTL zero caches nothing" do
    cache = CountCache.new(0)
    calls = 0
    2.times { cache.get_or_count(:books) { calls += 1 } }
    assert_equal 2, calls
  end

  test "keys are independent, a nil key always counts, a failed count is not cached" do
    cache = CountCache.new(60)
    assert_equal 1, cache.get_or_count(:books) { 1 }
    assert_equal 2, cache.get_or_count(:members) { 2 }
    calls = 0
    2.times { cache.get_or_count(nil) { calls += 1 } }
    assert_equal 2, calls
    assert_raises(RuntimeError) { cache.get_or_count(:loans_all) { raise "boom" } }
    assert_equal 3, cache.get_or_count(:loans_all) { 3 }
  end

  test "when an entry expires under load one thread counts and the others wait for it" do
    cache = CountCache.new(60)
    calls = Concurrent::AtomicFixnum.new
    threads = Array.new(6) { Thread.new { cache.get_or_count(:books) { calls.increment; sleep 0.1; 5 } } }
    assert_equal [5] * 6, threads.map(&:value)
    assert_equal 1, calls.value
  end
end
