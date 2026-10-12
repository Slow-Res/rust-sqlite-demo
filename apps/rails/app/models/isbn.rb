# A syntactically valid ISBN-10 or ISBN-13, stored without hyphens or spaces and
# with an uppercase X check digit.
module Isbn
  INVALID = "must be a valid ISBN-10 or ISBN-13"

  # The normalized ISBN, or nil when `input` is not a valid one.
  def self.normalize(input)
    isbn = input.delete("-").gsub(/[[:space:]]/, "").upcase(:ascii)
    case isbn.bytesize
    when 10 then isbn if valid10?(isbn)
    when 13 then isbn if valid13?(isbn)
    end
  end

  # Nine digits plus a digit or X (= 10); weights 10 down to 1; sum divisible by 11.
  def self.valid10?(isbn)
    return false unless isbn.match?(/\A[0-9]{9}[0-9X]\z/)

    sum = isbn.each_char.with_index.sum { |char, i| (char == "X" ? 10 : char.to_i) * (10 - i) }
    (sum % 11).zero?
  end

  # Thirteen digits; weights alternate 1, 3; sum divisible by 10.
  def self.valid13?(isbn)
    return false unless isbn.match?(/\A[0-9]{13}\z/)

    sum = isbn.each_char.with_index.sum { |char, i| char.to_i * (i.even? ? 1 : 3) }
    (sum % 10).zero?
  end
end
