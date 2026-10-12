# List parameters and the page envelope (spec 5.8): `page` / `per_page`, the
# keyset cursor `after`, the search term `q`, and `next_cursor`.
module Pagination
  DEFAULT_PER_PAGE = 20
  MAX_PER_PAGE = 100
  MAX_SEARCH_CHARS = 200

  # `keyset`: an `after` cursor was given, so `page` is ignored and the offset is 0.
  Params = Data.define(:page, :per_page, :keyset) do
    # One row more than the page, to learn whether a next page exists.
    def limit = per_page + 1

    def offset = keyset ? 0 : (page - 1) * per_page
  end

  # Defaults and clamping instead of rejecting out-of-range values, plus the
  # decoded cursor (`[key, id]`, the key being a String or an Integer) if one was
  # given.
  def self.params(page, per_page, after, key_type)
    cursor = after && decode_cursor(after, key_type)
    params = Params.new(page: [page || 1, 1].max,
                        per_page: (per_page || DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE),
                        keyset: !cursor.nil?)
    [params, cursor]
  end

  # Trim a search term; blank means no filter; too long is a 422 on `q`.
  def self.search_term(raw)
    q = raw && Text.trim(raw)
    return nil if q.nil? || q.empty?
    raise ApiError.validation("q" => "must be at most 200 characters") if q.length > MAX_SEARCH_CHARS

    q
  end

  # The cursor of a keyset position (the sort key and the id of the last item):
  # base64url, no padding, of the JSON `[key, id]`.
  def self.encode_cursor(key, id)
    Base64.urlsafe_encode64(JSON.generate([key, id]), padding: false)
  end

  # Anything that is not what encode_cursor made for this key type is a 400.
  def self.decode_cursor(cursor, key_type)
    json = Base64.urlsafe_decode64(cursor)
    # Strict: no padding, no characters outside the alphabet, no stray bits.
    raise ArgumentError unless Base64.urlsafe_encode64(json, padding: false) == cursor

    parsed = JSON.parse(json)
    raise ArgumentError unless parsed.is_a?(Array) && parsed.size == 2

    key, id = parsed
    valid_key = key_type == :string ? key.is_a?(String) : key.is_a?(Integer) && int64?(key)
    raise ArgumentError unless valid_key && id.is_a?(Integer) && int64?(id)

    [key, id]
  rescue ArgumentError, JSON::ParserError, EncodingError
    raise ApiError.bad_request("invalid cursor")
  end

  def self.int64?(value) = value.between?(-(2**63), 2**63 - 1)

  # The JSON page for `records` (fetched with `limit`): cut to the page, with the
  # cursor of the last item when a row was left over. `cursor_key` gives the sort
  # key of a record; the block renders one item.
  def self.envelope(params, records, total, cursor_key:)
    more = records.size > params.per_page
    items = records.first(params.per_page)
    next_cursor = more ? encode_cursor(cursor_key.call(items.last), items.last.id) : nil
    { items: items.map { |record| yield record }, page: params.page, per_page: params.per_page,
      total: total, next_cursor: next_cursor }
  end
end
