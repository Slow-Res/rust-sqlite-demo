# The Rust app writes `<`, `>` and `&` as they are; Rails would write < etc.
ActiveSupport::JSON::Encoding.escape_html_entities_in_json = false
