# String rules the Rust app gets from std: Unicode-aware trim, the words of a
# search term, LIKE escaping.
module Text
  # `str::trim`: Unicode White_Space at both ends (Ruby's `strip` only does ASCII).
  def self.trim(text) = text.gsub(/\A[[:space:]]+|[[:space:]]+\z/, "")

  # The words of a book search `q`: runs of letters and digits, so whitespace and
  # punctuation separate words and none of it reaches a query.
  def self.search_words(text) = text.scan(/[\p{Alphabetic}\p{N}]+/)

  # `q` with spaces and hyphens removed, when that can be the start of an ISBN and
  # differs from `q` ("978-0-261" finds "9780261..."). nil when `q` is a single
  # word already or holds anything but ASCII letters and digits.
  def self.isbn_prefix(text)
    digits = text.delete(" ").delete("-")
    digits if digits != text && !digits.empty? && digits.match?(/\A[A-Za-z0-9]+\z/)
  end

  # Escape %, _ and \ so user text matches literally inside LIKE ... ESCAPE '\'.
  def self.escape_like(text) = text.gsub(/[%_\\]/) { |char| "\\#{char}" }
end
