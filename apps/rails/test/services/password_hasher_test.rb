require "test_helper"

class PasswordHasherTest < ActiveSupport::TestCase
  test "verifies a hash made by the Rust app" do
    assert PasswordHasher.verify(PASSWORD, PASSWORD_HASH)
    assert_not PasswordHasher.verify("wrong", PASSWORD_HASH)
  end

  test "the dummy hash is a valid hash" do
    assert_not PasswordHasher.verify("anything", PasswordHasher::DUMMY_HASH)
  end

  test "a malformed hash raises" do
    assert_raises(StandardError) { PasswordHasher.verify("x", "not a hash") }
  end

  test "at most one hash at a time, across threads" do
    running = Concurrent::AtomicFixnum.new
    peak = Concurrent::AtomicFixnum.new
    threads = Array.new(5) do
      Thread.new do
        PasswordHasher.exclusively do
          peak.update { |max| [max, running.increment].max }
          sleep 0.03
          running.decrement
        end
      end
    end
    threads.each(&:join)
    assert_equal 1, peak.value
  end

  test "the lock is a flock, so other processes wait too" do
    File.open(PasswordHasher::LOCK_PATH, File::RDWR | File::CREAT, 0o600) do |file|
      file.flock(File::LOCK_EX)
      other = File.open(PasswordHasher::LOCK_PATH, File::RDWR)
      assert_equal false, other.flock(File::LOCK_EX | File::LOCK_NB)
      other.close
    end
  end
end
