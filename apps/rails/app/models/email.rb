# A syntactically plausible email address, trimmed and lowercased so that
# uniqueness in the database is effectively case-insensitive.
module Email
  INVALID = "must be a valid email address"
  TOO_LONG = "must be at most 254 characters"

  # `[email, nil]` or `[nil, message]`. At most 254 characters, exactly one `@`, a
  # non-empty local part, and a domain with a `.` that is neither its first nor
  # its last character.
  def self.parse(input)
    email = Text.trim(input).downcase
    return [nil, TOO_LONG] if email.length > 254

    local, domain, *rest = email.split("@", -1)
    dot_inside_domain = domain&.include?(".") && !domain.start_with?(".") && !domain.end_with?(".")
    return [nil, INVALID] if domain.nil? || rest.any? || local.empty? || !dot_inside_domain

    [email, nil]
  end
end
