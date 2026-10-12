# Argon2id verification of the PHC strings the Rust app stored
# ($argon2id$v=19$m=19456,t=2,p=1$...), one at a time.
#
# A hash takes 19 MiB and 50-100 ms of CPU, so a login burst must not run many at
# once. The Rust app has one permit for the whole process; here there are several
# processes (Puma workers), so the permit is an exclusive `flock` on a file: at
# most one hash runs at a time across every worker and thread, the others wait
# for the lock. The argon2 gem releases the GVL while it hashes, so the other
# threads of the worker keep serving.
module PasswordHasher
  # A valid hash of a throwaway password. Login verifies against it when the
  # username is unknown, so the response time does not reveal which usernames exist.
  DUMMY_HASH = "$argon2id$v=19$m=19456,t=2,p=1$kULZ+KhiNP3f8wTuk3Haug$bbmpS3ahQNEAWvNyloOuFiyTw9u6U2egWfUVB+aftxE".freeze

  LOCK_PATH = ENV.fetch("PASSWORD_HASH_LOCK", "/tmp/library-password-hash.lock")

  # True if `password` matches `hash`, false if it does not. A malformed hash raises.
  def self.verify(password, hash)
    exclusively { Argon2::Password.verify_password(password, hash) }
  end

  # Run the block holding the lock. Each call opens the file itself, so threads of
  # one process exclude each other as well as other processes (a lock belongs to the
  # open file, and closing the file releases it, whatever the block did).
  def self.exclusively
    File.open(LOCK_PATH, File::RDWR | File::CREAT, 0o600) do |file|
      file.flock(File::LOCK_EX)
      yield
    end
  end
end
