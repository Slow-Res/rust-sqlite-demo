# API contract

The HTTP API of the library app, as the Rust app (`apps/web/src`) serves it today. The
Rust app is the reference: this document describes it, it does not change it. A second
implementation (the Rails copy, `docs/superpowers/specs/2026-10-13-rails-comparison-design.md`)
must answer every request the way this document says, and `scripts/contract-test.sh`
checks a running box against it (`docs/LOADTEST.md`, "Contract test").

Sources: the routes, handlers, services and repository SQL in `apps/web/src`, the DTOs in
`crates/shared/src`, the Postgres schema in `migrations/web-postgres`, and the production
rules in `docs/superpowers/specs/2026-10-10-sqlite-production-design.md` §5. (The
comments in the code that cite "spec §5.5", "§5.7" and "§5.8" refer to the original
library-app design, not to that file; this document is where those rules are written
down now.)

**Only Postgres matters for Rails.** The app runs on SQLite or Postgres. Where they differ
it is stated under "Differences between SQLite and Postgres" at the end; everything else
is the same on both, and the SQL given here is the Postgres SQL.

Contents: 1 Conventions, 2 Errors, 3 Session cookie, 4 Paging, 5 Totals and their cache,
6 Routes (health, auth, books, members, loans), 7 Search, 8 The Postgres SQL,
9 Locking and transactions, 10 Differences between SQLite and Postgres.

## 1. Conventions

- Every route is under `/api`. Bodies and responses are JSON (`Content-Type:
  application/json`), UTF-8. Responses may be gzip-compressed when the client sends
  `Accept-Encoding`.
- **Timestamps** are Unix seconds, UTC, stored as `BIGINT`. In JSON they are RFC 3339 UTC
  with a `Z` suffix and whole seconds only: `2026-10-13T12:34:56Z` (never fractions, never
  an offset). "Now" is the wall clock truncated to whole seconds, read once per request.
- **Ids** are positive 64-bit integers (`BIGINT` identity), JSON numbers. Counts and
  totals are JSON integers.
- **Strings** in request bodies are trimmed before validation and storage. Lengths count
  Unicode characters, not bytes.
- Unknown JSON fields in a request body are ignored. `null` is allowed only where this
  document says the field is nullable; a missing key counts as `null` for the two
  nullable book fields (`isbn`, `published_year`) and is an error for every other field.
- **Headers on every response** (also errors): `X-Content-Type-Options: nosniff`,
  `X-Frame-Options: DENY`, `Referrer-Policy: same-origin` (the 405 and 413 answers are produced by
  outer layers and may lack them; the contract does not require them there), and `X-Request-Id` (a UUID, generated per request, or
  the one the client sent).
- **Limits:** request body at most 65536 bytes (413 beyond); a request that takes longer
  than 15 s is answered 408.
- **Order in which a request is judged:** route lookup (unknown path: 404; known path,
  other method: 405) -> session (401) -> the path id (400) -> the body or query string
  (400) -> field validation (422) -> existence and business rules (404, 409). So an
  unauthenticated request is 401 whatever its body, and a `PUT` with an invalid body is
  422 even if the id does not exist.
- Trailing slashes are not tolerated: `/api/books/` is an unknown route (404).
- A query-string parameter that is present but empty is not "absent": `availability=`
  and `page=` are 400 (empty is not a valid enum value or number). `q=` empty or blank is
  no filter.

## 2. Errors

Every non-2xx answer of a `/api/*` route has the same body (`library_shared::ApiError`):

```json
{"error": {"code": "validation_error", "message": "Some fields are invalid",
           "fields": {"title": "must not be empty"}}}
```

`code` and `message` are always present strings; `fields` only for `validation_error`
(object of field name -> one message, keys in alphabetical order, the first message for
a field wins; present only when non-empty). The content type is `application/json`.

| Status | `code` | `message` | When |
|---|---|---|---|
| 400 | `bad_request` | free text of the framework | malformed JSON, missing or mistyped field, no `Content-Type: application/json`, a body that is not an object, a query or path parameter that does not parse (`page=abc`, `/books/abc`, an unknown enum value), a bad cursor. The text is the web framework's and is **not** part of the contract (clients and the contract test only look at status and code), except `invalid cursor`, which is exact |
| 401 | `unauthorized` | `Authentication required` | no session cookie, a cookie that is no live session |
| 401 | `unauthorized` | `invalid username or password` | failed login (same text whether the user exists or not) |
| 404 | `not_found` | `<entity> not found` | `book not found`, `member not found`, `loan not found`; `route not found` for any unknown `/api/...` path with any method |
| 405 | `method_not_allowed` | `method not allowed` | a known path, a method it does not have (an `Allow` header lists the right ones) |
| 408 | `request_timeout` | `request timed out` | no answer within 15 s |
| 409 | see the routes | see the routes | business rules: `isbn_taken`, `email_taken`, `no_copies_available`, `loan_limit_reached`, `copies_below_active_loans`, `book_has_active_loans`, `member_has_active_loans`, `loan_already_returned` |
| 413 | `payload_too_large` | `request body too large` | body over 65536 bytes |
| 422 | `validation_error` | `Some fields are invalid` | field validation failed; `fields` says which |
| 500 | `internal` | `Internal server error` | anything unexpected; the cause is logged, never sent |
| 503 | (health body, see 6.1) | | `GET /api/health` when the database does not answer |

`username_taken` (409) exists in the code for the `create-user` command; no HTTP route
returns it.

Examples of the exact 409 and 404 answers are in the route tables below.

## 3. Session cookie

- Name: `library_session`. Value: 32 random bytes from the OS RNG, hex-encoded: 64
  lowercase hex characters. The database stores only the SHA-256 of the token's 64 ASCII characters, as
  lowercase hex (64 characters), never the token.
- Set by `POST /api/auth/login` as exactly one `Set-Cookie`:
  `library_session=<token>; HttpOnly; SameSite=Lax; Path=/; Max-Age=<ttl seconds>`
  plus `Secure` when the app runs with `COOKIE_SECURE=true` (default false). No `Domain`,
  no `Expires`. Attribute names may differ in letter case.
- TTL: `SESSION_TTL_HOURS` (default 168 = 7 days, range 1..87600); `Max-Age` is the same
  in seconds (604800). The session row has `created_at = now`, `expires_at = now + ttl`.
  It is not extended by use (no sliding expiry).
- A request is authenticated when a row with the token's hash exists and
  `expires_at > now` (strictly). Anything else (no cookie, empty value, unknown token,
  expired row) is `401 unauthorized` / `Authentication required`.
- **Sweep:** every successful login, after creating its session, runs
  `DELETE FROM sessions WHERE expires_at <= now` (Postgres first checks
  `SELECT 1 FROM sessions WHERE expires_at <= $1 LIMIT 1` and skips the delete when
  nothing is expired). A failed sweep does not fail the login. Expired rows therefore
  stay in the table until the next login; they are already refused by the check above.
- `POST /api/auth/logout` deletes the session row and answers with a clearing cookie:
  `library_session=; Path=/; Max-Age=0` (and an `Expires` in the past).
- Several sessions per user may exist at once; logging out one leaves the others.

## 4. Paging

Applies to `GET /api/books`, `GET /api/members`, `GET /api/loans`.

### Query parameters

| Parameter | Type | Meaning |
|---|---|---|
| `page` | unsigned 32-bit integer | 1-based page for offset paging. Default 1; `0` is clamped to 1. Not a number, negative, or above 4294967295: 400 |
| `per_page` | unsigned 32-bit integer | Default **20**; clamped into `1..=100` (`0` becomes 1, `5000` becomes 100). Not a number or negative: 400 |
| `after` | string | A cursor (below). When given, `page` is ignored for choosing rows (offset 0), but the response still echoes the requested `page` (default 1) |
| `q` | string | Search text (books, members), see 7. Trimmed; blank means no filter; more than 200 characters (after trimming): 422 `{"q":"must be at most 200 characters"}` |
| `availability` | `all` \| `available` \| `on_loan` | Books only. Default `all` |
| `status` | `active` \| `overdue` \| `returned` \| `all` | Loans only. Default `active` |
| `member_id`, `book_id` | integer (i64) | Loans only. Exact filters, combined with AND. An id that matches nothing gives an empty page, not 404 |

Unknown query parameters are ignored.

### Response (`Page<T>`)

```json
{"items": [ ... ], "page": 1, "per_page": 20, "total": 2000, "next_cursor": "WyJB...XQ"}
```

Keys are exactly `items`, `page`, `per_page`, `total`, `next_cursor`.

- `items`: at most `per_page` rows in the order given for the list.
- `page`, `per_page`: the clamped values (`page` is echoed also with `after`).
- `total`: number of matching rows over all pages; see 5 for when it may lag.
- `next_cursor`: `null` when there is no row after this page, otherwise the cursor of the
  **last item of this page**. It is also set for plain `page` requests, so a client can
  switch to cursors after page 1. The service fetches `per_page + 1` rows: a next cursor
  exists exactly when that extra row came back, so a page that ends exactly at the last
  row has `next_cursor: null`.
- A page past the end: `items: []`, the exact `total`, `next_cursor: null`.

### Cursor format

Keyset paging: the cursor is the sort key and id of the last item.
`base64url_no_pad(compact_json([key, id]))`: URL-safe alphabet (`-` and `_`, no `+` `/`),
no `=` padding, of the compact JSON array (no spaces) `[key, id]`.

| List | `key` | Example (decoded) | Order, and "after" means |
|---|---|---|---|
| books | `title` (string) | `["Ancient Archive 1507",1506]` | `title ASC, id ASC`; `(title, id) > (key, id)` |
| members | `name` (string) | `["Ada Lovelace",12]` | `name ASC, id ASC`; `(name, id) > (key, id)` |
| loans, status `active`, `overdue`, `all` | `due_at` as Unix seconds (integer) | `[1793054586,7]` | `due_at ASC, id ASC`; `(due_at, id) > (key, id)` |
| loans, status `returned` | `returned_at` as Unix seconds | `[1791844987,999]` | `returned_at DESC, id DESC`; `(returned_at, id) < (key, id)` |

Strings compare bytewise (`COLLATE "C"`: `"B" < "a"`), so the order is the same as
SQLite's `BINARY`. Ties on the key are broken by `id`.

A cursor that is not valid base64url, not valid JSON, not a two-element array, or whose
key has the wrong type for the list (a loan cursor on `/books`: `[1793054586,7]` is not
`[string, int]`; a book cursor on `/loans`) is `400 bad_request` with message exactly
`invalid cursor`. The empty string is invalid. A well-formed cursor for a row that no
longer exists is fine: the page starts after that position.

The decoded values are bound as parameters (never put in SQL text).

## 5. Totals and their cache

The `total` of a list needs a count of the matching rows, which for the whole table is
expensive. So the totals of a few lists are cached **in the memory of the app process**
(one cache per process; with several workers each has its own):

| Cache key | List | SQL that fills it |
|---|---|---|
| `Books` | `GET /api/books` without `q`, `availability=all` | `SELECT COUNT(*) FROM books` |
| `BooksOnLoan` | `GET /api/books?availability=on_loan` without `q` | `SELECT COUNT(DISTINCT book_id) FROM loans WHERE returned_at IS NULL` |
| `BooksAvailable` | `GET /api/books?availability=available` without `q` | cached `Books` minus the number of full books (8.1), never below 0 |
| `Members` | `GET /api/members` without `q` | `SELECT COUNT(*) FROM members` |
| `LoansActive` | `GET /api/loans` with status `active` (the default), no `member_id` or `book_id` | `SELECT COUNT(*) FROM loans l WHERE 1 = 1 AND l.returned_at IS NULL` |
| `LoansReturned` | status `returned`, no `member_id` or `book_id` | `... AND l.returned_at IS NOT NULL` |
| `LoansAll` | status `all`, no `member_id` or `book_id` | `SELECT COUNT(*) FROM loans l WHERE 1 = 1` |

- **TTL:** `LIST_COUNT_TTL_MS`, default **1000** ms; `0` turns the cache off (every
  request counts, totals are exact). An entry is used while `now - taken_at < ttl`, where
  `taken_at` is when the count was requested (a slow count does not look fresher than
  it is).
- **Invalidation: none.** Writes do not clear the cache. The only way a cached total
  changes is that it expires. So after a write an unfiltered `total` may be stale for up
  to one TTL (with several processes, each process's cache expires on its own clock).
  Rows are never cached, only totals: the page of items is always read fresh.
- **Concurrency:** one lock per key, held while counting: when an entry expires under
  load, one request counts and the others wait for its answer.
- A failed count is not cached.
- **Never cached (always exact):** any list with `q` (books, members); loans with
  `member_id` or `book_id`; loans with status `overdue` (depends on the current time);
  an empty page of a word search (the count query runs).
- The same applies to `BooksAvailable`: it is a derived value (cached `Books` - full
  books) and is cached under its own key too; the cached `Books` value it reads is at
  most one TTL old, the full-books count is read fresh when `BooksAvailable` is filled.

## 6. Routes

Summary:

| Method | Path | Auth |
|---|---|---|
| GET | `/api/health` | none |
| POST | `/api/auth/login` | none |
| POST | `/api/auth/logout` | session |
| GET | `/api/auth/me` | session |
| GET | `/api/books` | none |
| POST | `/api/books` | session |
| GET | `/api/books/{id}` | none |
| PUT | `/api/books/{id}` | session |
| DELETE | `/api/books/{id}` | session |
| GET | `/api/members` | session |
| POST | `/api/members` | session |
| GET | `/api/members/{id}` | session |
| PUT | `/api/members/{id}` | session |
| DELETE | `/api/members/{id}` | session |
| GET | `/api/loans` | session |
| POST | `/api/loans` | session |
| POST | `/api/loans/{id}/return` | session |
| any | any other `/api/...` | none: 404 `route not found` |

There is no `GET /api/loans/{id}` and no loan delete (404 and 405 as per 2). Reads of
books are public; every write and everything about members and loans needs a session.
`{id}` is a signed 64-bit integer in decimal; anything else (`abc`, `1.5`, an integer
beyond 64 bits) is 400; `0` and negative numbers are valid ids that do not exist (404).

### 6.1 `GET /api/health`

No auth (a bad cookie is ignored). Runs a database round trip.

- 200 `{"status":"ok","node_id":"node1","version":"0.1.0"}`: exactly these three keys;
  `node_id` the node's configured name (non-empty), `version` the app's version string.
- 503 with the same shape and `"status":"unavailable"` when the database does not answer.

### 6.2 Auth

**`POST /api/auth/login`**, body `{"username": string, "password": string}` (both required).

1. Validation (422, `fields`): `username` blank after trimming: `"must not be empty"`;
   `password` empty (not trimmed; spaces count): `"must not be empty"`.
2. The username is trimmed and looked up **case-insensitively** (ASCII folding):
   `SELECT id, username, password_hash FROM users WHERE lower(username) = lower($1)`.
3. A password hash is **always** verified, even for an unknown user (against a fixed
   dummy Argon2id hash), so both failures take the same time. The password hashes are
   Argon2id PHC strings (`$argon2id$v=19$m=19456,t=2,p=1$...`, 19 MiB, 2 passes, 1 lane,
   the Rust `argon2` crate defaults). Hashing and verifying are limited to
   `PASSWORD_HASH_PERMITS` at a time across the whole app (default half the cores, at
   least 1: one on the 2-core box).
4. Wrong password or unknown user: 401 `unauthorized`, message
   `invalid username or password` (same for both), no cookie.
5. Success: 200, body `{"id": 1, "username": "admin"}` (exactly `id` and `username`; the
   username as stored, not as typed), `Set-Cookie` as in 3. Creates the session row
   (`INSERT INTO sessions (token_hash, user_id, created_at, expires_at)`), then sweeps
   expired sessions.

Missing `password` key, non-string values, malformed JSON, wrong or missing content type:
400.

**`POST /api/auth/logout`**, no body. Needs a session (401 otherwise, also for a second
logout of the same token). Deletes the session (`DELETE FROM sessions WHERE token_hash = $1`).
204, empty body, `Set-Cookie` clearing the cookie (3).

**`GET /api/auth/me`**: 200 `{"id": 1, "username": "admin"}` of the logged-in user.

### 6.3 Books

Book JSON (`Book`), exactly these keys:

```json
{"id": 2001, "title": "Dune", "author": "Frank Herbert", "isbn": "9780306406157",
 "published_year": 1965, "total_copies": 3, "available_copies": 2,
 "created_at": "2026-10-13T09:00:00Z", "updated_at": "2026-10-13T09:00:00Z"}
```

| Field | Type | Notes |
|---|---|---|
| `id` | integer | |
| `title`, `author` | string | trimmed |
| `isbn` | string or `null` | normalized: no hyphens or spaces, uppercase `X`; `null` when none |
| `published_year` | integer or `null` | |
| `total_copies` | integer | 1..1000 |
| `available_copies` | integer | `total_copies` minus the book's unreturned loans, computed at read time; never stored |
| `created_at`, `updated_at` | timestamp | on create both are `now`; `PUT` sets `updated_at = now`, keeps `created_at` |

**Request body** of `POST /api/books` and `PUT /api/books/{id}` (`BookInput`):
`{"title": string, "author": string, "isbn": string|null, "published_year": integer|null, "total_copies": integer}`.
`title`, `author`, `total_copies` are required keys (missing or wrong JSON type: 400;
`total_copies` must be a JSON integer: `"1"` and `1.5` are 400; `published_year` must fit
a signed 32-bit integer or it is 400). `isbn` and `published_year` may be `null` or absent.

**Validation (422, all bad fields reported together):**

| Field | Rule | Message |
|---|---|---|
| `title` | after trimming: 1..300 characters | `must not be empty` (blank) / `must be at most 300 characters` |
| `author` | after trimming: 1..200 characters | `must not be empty` / `must be at most 200 characters` |
| `isbn` | after trimming, `null` or blank means no ISBN (stored `NULL`). Otherwise remove every `-` and whitespace character, uppercase; it must be 10 or 13 characters and pass the checksum: ISBN-10: nine digits and a digit or `X` last, weights 10 down to 1, sum divisible by 11; ISBN-13: thirteen digits, weights 1,3,1,3,..., sum divisible by 10 | `must be a valid ISBN-10 or ISBN-13` |
| `published_year` | `null`, or `1..=current_year + 1` (current year of the server clock, UTC) | `must be a valid year` |
| `total_copies` | `1..=1000` | `must be between 1 and 1000` |

Example, everything wrong: `{"error":{"code":"validation_error","message":"Some fields are invalid","fields":{"author":"must not be empty","isbn":"must be a valid ISBN-10 or ISBN-13","published_year":"must be a valid year","title":"must not be empty","total_copies":"must be between 1 and 1000"}}}`.

**Routes**

- `GET /api/books` (public): query `q`, `availability`, `page`, `per_page`, `after` (4);
  200 `Page<Book>`, ordered `title ASC, id ASC` (bytewise); search in 7;
  availability: `available` = `total_copies - active_loans > 0`, `on_loan` =
  `active_loans > 0` (a partly loaned book is both; a full book is only `on_loan`; a book
  with no loan is only `available`), `all` = everything. `q` and `availability` combine.
- `POST /api/books` (session): 201 + the created `Book`. 409 `isbn_taken` /
  `a book with this ISBN already exists` when another book has the ISBN (compared in
  normalized form: `978-0-306-40615-7` equals `9780306406157`). 422, 400 as above.
- `GET /api/books/{id}` (public): 200 `Book`; 404 `not_found` / `book not found`.
- `PUT /api/books/{id}` (session): replaces all five input fields (not partial). 200 +
  the updated `Book`. Validation (422) runs before the existence check. 404 `book not found`.
  409 `isbn_taken` (same message) when the new ISBN belongs to another book; the
  book's own ISBN is no conflict. 409 `copies_below_active_loans` /
  `total copies cannot be less than the copies currently on loan` when the new
  `total_copies` is below the number of the book's unreturned loans (equal is fine).
- `DELETE /api/books/{id}` (session): 204, empty body. The book's **returned** loans are
  deleted with it (`ON DELETE CASCADE`). 404 `book not found`; 409
  `book_has_active_loans` / `book has active loans and cannot be deleted` when an
  unreturned loan exists.

### 6.4 Members

Member JSON (`Member`), exactly these keys:

```json
{"id": 12, "name": "Ada Lovelace", "email": "ada@example.com", "active_loans": 1,
 "created_at": "2026-10-13T09:00:00Z", "updated_at": "2026-10-13T09:00:00Z"}
```

`active_loans` is the number of the member's unreturned loans, computed at read time.
`email` is stored and returned **lowercased and trimmed**.

**Request body** (`MemberInput`) of `POST /api/members` and `PUT /api/members/{id}`:
`{"name": string, "email": string}`, both required keys and strings.

**Validation (422):**

| Field | Rule | Message |
|---|---|---|
| `name` | after trimming 1..200 characters | `must not be empty` / `must be at most 200 characters` |
| `email` | trimmed and lowercased; at most 254 characters; exactly one `@`; non-empty part before it; the part after it contains a `.` that is neither its first nor its last character | longer than 254: `must be at most 254 characters`; any other failure (including empty): `must be a valid email address` |

Both fields are always checked: a bad name and a bad email give two entries.

**Routes** (all need a session)

- `GET /api/members`: query `q`, `page`, `per_page`, `after`; `Page<Member>` ordered
  `name ASC, id ASC` (bytewise); search in 7.
- `POST /api/members`: 201 + `Member`; 409 `email_taken` /
  `a member with this email already exists` (emails compare case-insensitively because
  they are stored lowercased).
- `GET /api/members/{id}`: 200; 404 `member not found`.
- `PUT /api/members/{id}`: 200 + `Member` (updated_at = now); 422 before 404; 404; 409
  `email_taken` for another member's email (a member keeping its own email is fine).
- `DELETE /api/members/{id}`: 204; the member's returned loans go with it (cascade); 404;
  409 `member_has_active_loans` / `member has active loans and cannot be deleted`.

### 6.5 Loans

Loan JSON (`Loan`), exactly these keys:

```json
{"id": 7, "book_id": 11, "book_title": "Wild Harbor 25", "member_id": 11,
 "member_name": "Karim Zielinski 26", "borrowed_at": "2026-10-12T22:43:06Z",
 "due_at": "2026-10-26T22:43:06Z", "returned_at": null, "is_overdue": false}
```

`book_title` and `member_name` are the book's and member's **current** title and name
(joined at read time). `returned_at` is `null` until returned. `is_overdue` is
`returned_at IS NULL AND due_at < now` (strictly: due exactly now is not overdue yet),
computed with the request's `now`; a returned loan is never overdue.

**Routes** (all need a session)

- `GET /api/loans`: query `status` (default `active`), `member_id`, `book_id`, `page`,
  `per_page`, `after`. `Page<Loan>`. Filters: `active`: unreturned; `overdue`: unreturned
  and `due_at < now`; `returned`: returned; `all`: no filter on the state. `member_id` and
  `book_id` add exact `AND` filters. Order: `due_at ASC, id ASC` for `active`, `overdue`
  and `all`; `returned_at DESC, id DESC` for `returned`.
- `POST /api/loans`, body `{"book_id": integer, "member_id": integer}` (both required
  JSON integers): creates a loan with `borrowed_at = now`, `due_at = now + 14 days`
  (exactly 1209600 seconds). 201 + `Loan`. Validation (422): `book_id` below 1:
  `{"book_id":"must be a positive id"}`; `member_id` below 1: `{"member_id":"must be a positive id"}`
  (both reported together). Then the rules are checked **in this order, first failure wins**:
  1. the book exists, else 404 `book not found`;
  2. the member exists, else 404 `member not found`;
  3. a copy is free (unreturned loans of the book < `total_copies`), else 409
     `no_copies_available` / `no copies of this book are available`;
  4. the member has fewer than **5** unreturned loans, else 409 `loan_limit_reached` /
     `member already has the maximum of 5 active loans`.
  The same member may borrow the same book again while a copy is free.
- `POST /api/loans/{id}/return`, no body: sets `returned_at = now` and answers 200 + the
  `Loan` (`is_overdue` false). 404 `loan not found`; 409 `loan_already_returned` /
  `loan has already been returned` when it is already returned.

## 7. Search and availability semantics

### Books: `q` (word-prefix search)

`q` is trimmed (blank: no filter; at most 200 characters, else 422 on `q`). Its **words**
are the maximal runs of alphanumeric characters (Unicode `is_alphanumeric`: letters and
digits; everything else, such as spaces, `-`, `'`, `.`, `"`, `*`, `:`, `(`, `_`, separates
words). A book matches when **every** word of `q` is a **prefix of some word** of its title,
author or ISBN, where the document's words are produced the same way (title, author and
ISBN joined by spaces; ASCII punctuation becomes a space) and compared
case-insensitively. A word matches from its start: `quill` finds "Quillfeather", `eather`
does not. The words may come from different columns.

Examples: `q=anc ada` finds "Ancient Anchor" by "Ada Lovelace"; `q=brien` finds an author
"O'Brien" (words `o`, `brien`); `q=obrien` finds nothing.

ISBN extras: the ISBN is one word (digits and `X`, normalized). So `q=9780306` finds a
book whose ISBN starts with it. A `q` typed with separators, such as `978-0-306-40` or
`978 0306`, is also tried as an ISBN prefix: when `q` with every space and hyphen removed
differs from `q`, is non-empty and is only ASCII letters and digits, the book also
matches when `isbn LIKE '<that text uppercased>%'`. (That is OR-ed with the word match.)

A `q` with **no** word (only punctuation, for example `##` or `%`): a plain substring match,
case-insensitive (ASCII), on title, author or ISBN: `title ILIKE '%q%' OR author ILIKE
'%q%' OR isbn ILIKE '%q%'` with `\`, `%` and `_` in `q` escaped (`ESCAPE '\'`), and when
`q` without spaces and hyphens is non-empty also `OR isbn ILIKE '%that%'`. A query of
`%` therefore matches only text containing a literal percent sign.

`q` is never interpreted as search syntax: quotes, `*`, `:`, `&`, `|`, `!`, `(` and so on
cannot break the query, and never give an error.

Case folding in the word search is the database's (`to_tsvector('simple', ...)` lowercases;
on Postgres with the `C` collation only ASCII letters fold reliably; SQLite folds all of
Unicode). The contract test uses ASCII words only.

With `availability`, the book must also satisfy the availability predicate (8.1). The
search total is exact (never cached) and counts all matches, not only those after a cursor.

### Members: `q` (substring search)

`name ILIKE '%q%' ESCAPE '\' OR email ILIKE '%q%' ESCAPE '\'` with `\`, `%` and `_` in `q`
escaped. A substring, not a word prefix; case-insensitive for ASCII; `q` may contain spaces;
`%` and `_` are literal. Exact total (not cached).

## 8. The Postgres SQL

Text columns are `COLLATE "C"`; timestamps are `BIGINT` Unix seconds; `$n` are bound
parameters. Fragments are given as the Rust app builds them (a `QueryBuilder` appending
to a base). The table definitions are `migrations/web-postgres/*.sql`: Rails runs no
migrations, it maps onto those tables.

### 8.1 Books

Shared pieces:

```sql
-- active_loans(b): the book's unreturned loans
(SELECT COUNT(*) FROM loans l WHERE l.book_id = b.id AND l.returned_at IS NULL)

-- book columns, one row of a Book
SELECT b.id, b.title, b.author, b.isbn, b.published_year, b.total_copies,
       b.total_copies - (SELECT COUNT(*) FROM loans l WHERE l.book_id = b.id AND l.returned_at IS NULL) AS available_copies,
       b.created_at, b.updated_at

-- filter availability = available:   AND b.total_copies - active_loans(b) > 0
-- filter availability = on_loan:     AND active_loans(b) > 0

-- the search document and its GIN index (migration 20261014000000)
CREATE FUNCTION book_search_vector(title text, author text, isbn text) RETURNS tsvector
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
  SELECT to_tsvector('simple', regexp_replace(
      title || ' ' || author || ' ' || coalesce(isbn, ''),
      '[^0-9A-Za-z\u0080-\U0010FFFF]+', ' ', 'g'))
$$;
CREATE INDEX idx_books_search ON books USING gin (book_search_vector(title, author, isbn));
```

`GET /api/books`, **without a word search** (no `q`, or `q` with no words), page query
("deferred join": the inner query walks the index and skips `OFFSET` rows reading only
ids; the columns and the subquery are computed for the page's rows only):

```sql
<book columns> FROM (
  SELECT b.id FROM books b
  WHERE 1 = 1
    [AND <search filter without words>]          -- the ILIKE form of 7
    [AND <availability filter>]
    [AND (b.title, b.id) > ($t, $i)]             -- with `after`
  ORDER BY b.title ASC, b.id ASC
  LIMIT $per_page_plus_1 OFFSET $offset          -- offset 0 with `after`
) p JOIN books b ON b.id = p.id
ORDER BY b.title ASC, b.id ASC
```

`LIMIT` is `per_page + 1`; `OFFSET` is `(page - 1) * per_page` (computed in 64 bits).
The total is a **separate** count, run before the page query, through the cache of 5:

```sql
SELECT COUNT(*) FROM books                                   -- no q, availability=all   (cache Books)
SELECT COUNT(DISTINCT book_id) FROM loans WHERE returned_at IS NULL   -- on_loan, no q    (cache BooksOnLoan)
-- available, no q (cache BooksAvailable):  cached Books  -  this, never below 0
SELECT COUNT(*) FROM (SELECT book_id, COUNT(*) AS n FROM loans WHERE returned_at IS NULL GROUP BY book_id) a
  JOIN books b ON b.id = a.book_id WHERE b.total_copies - a.n <= 0
-- anything with q and no words (not cached):
SELECT COUNT(*) FROM books b WHERE 1 = 1 AND <the same filters as the page query>
```

`GET /api/books` with a **word search** (`q` has at least one word). The matches are found
through the GIN index in a `MATERIALIZED` CTE (the planner must not fold it into a walk
over the title order: the matches of one word cluster in title order, so that plan can
read most of the table to find 21 rows). The total comes from the same statement as a
window over the whole match set, so it counts every match, and the cursor condition comes
**after** the window:

```sql
<book columns>, p.total FROM (
  WITH m AS MATERIALIZED (
    SELECT b.id, b.title FROM books b
    WHERE 1 = 1
      AND (book_search_vector(b.title, b.author, b.isbn) @@ to_tsquery('simple', $tsquery)
           [OR b.isbn LIKE $isbn_prefix_upper_percent])
      [AND <availability filter>]
  )
  SELECT id, title, total FROM (SELECT id, title, COUNT(*) OVER () AS total FROM m) x
  [WHERE (x.title, x.id) > ($t, $i)]
  ORDER BY title ASC, id ASC
  LIMIT $per_page_plus_1 OFFSET $offset
) p JOIN books b ON b.id = p.id
ORDER BY b.title ASC, b.id ASC
```

`$tsquery` is the words as quoted prefixes joined by `&`: `'anc':* & 'ada':*`. The total of
the response is `p.total` of the first row. When the page is empty (past the end, no match, or
a cursor beyond the last match) there is no row to carry it, so the total is counted:
`SELECT COUNT(*) FROM books b WHERE 1 = 1 AND <search and availability filters>`.

One book: `<book columns> FROM books b WHERE b.id = $1`.

Create: `INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$6) RETURNING id` (isbn `NULL` when none), then the one-book select. A
violation of the unique index `uq_books_isbn` is the `isbn_taken` 409.

Update (transaction, 9): `SELECT id FROM books WHERE id = $1 FOR UPDATE` (none: 404), then
`SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL` (new `total_copies` below it:
409), then `UPDATE books SET title=$1, author=$2, isbn=$3, published_year=$4, total_copies=$5, updated_at=$6 WHERE id=$7`,
then the one-book select, commit. `uq_books_isbn` violation: 409 `isbn_taken`.

Delete (transaction): `SELECT id FROM books WHERE id = $1 FOR UPDATE` (none: 404), the same
active-loan count (above 0: 409), `DELETE FROM books WHERE id = $1` (returned loans cascade), commit.

### 8.2 Members

```sql
-- member columns
SELECT m.id, m.name, m.email,
       (SELECT COUNT(*) FROM loans l WHERE l.member_id = m.id AND l.returned_at IS NULL) AS active_loans,
       m.created_at, m.updated_at

-- page
<member columns> FROM (
  SELECT m.id FROM members m
  WHERE 1 = 1
    [AND (m.name ILIKE $p ESCAPE '\' OR m.email ILIKE $p ESCAPE '\')]   -- $p = '%' || escaped q || '%'
    [AND (m.name, m.id) > ($n, $i)]
  ORDER BY m.name ASC, m.id ASC
  LIMIT $per_page_plus_1 OFFSET $offset
) p JOIN members m ON m.id = p.id
ORDER BY m.name ASC, m.id ASC

-- total, run before the page, cached (Members) only without q
SELECT COUNT(*) FROM members m [WHERE 1 = 1 AND (m.name ILIKE ... OR m.email ILIKE ...)]
```

One member: `<member columns> FROM members m WHERE m.id = $1`.
Create: `INSERT INTO members (name, email, created_at, updated_at) VALUES ($1,$2,$3,$3) RETURNING id`
(email already lowercased), then the select; unique index `uq_members_email` violation: 409
`email_taken`. Update: one statement `UPDATE members SET name=$1, email=$2, updated_at=$3 WHERE id=$4`
(0 rows: 404), then the select; same 409. Delete (transaction): `SELECT id FROM members WHERE id = $1 FOR UPDATE`
(none: 404), `SELECT COUNT(*) FROM loans WHERE member_id = $1 AND returned_at IS NULL` (above 0: 409),
`DELETE FROM members WHERE id = $1` (cascade), commit.

### 8.3 Loans

```sql
-- loan columns, one Loan row
SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name,
       l.borrowed_at, l.due_at, l.returned_at
FROM loans l JOIN books b ON b.id = l.book_id JOIN members m ON m.id = l.member_id

-- status filter (after WHERE 1 = 1):
--   active:   AND l.returned_at IS NULL
--   overdue:  AND l.returned_at IS NULL AND l.due_at < $now
--   returned: AND l.returned_at IS NOT NULL
--   all:      (nothing)
-- then:  [AND l.member_id = $m]  [AND l.book_id = $b]

-- page (order is `due_at ASC, id ASC`, or for returned `returned_at DESC, id DESC`)
SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name,
       l.borrowed_at, l.due_at, l.returned_at
FROM (
  SELECT l.id FROM loans l
  WHERE 1 = 1 <status filter> [AND l.member_id = $m] [AND l.book_id = $b]
    [AND (l.due_at, l.id) > ($k, $i)]            -- with `after`; for returned:  (l.returned_at, l.id) < ($k, $i)
  ORDER BY l.due_at ASC, l.id ASC                -- for returned: ORDER BY l.returned_at DESC, l.id DESC
  LIMIT $per_page_plus_1 OFFSET $offset
) p JOIN loans l ON l.id = p.id JOIN books b ON b.id = l.book_id JOIN members m ON m.id = l.member_id
ORDER BY <same order>

-- total, before the page, cached (LoansActive / LoansReturned / LoansAll) when there is no
-- member_id, book_id and the status is not overdue
SELECT COUNT(*) FROM loans l WHERE 1 = 1 <status filter> [AND l.member_id = $m] [AND l.book_id = $b]
```

One loan: the loan columns with `WHERE l.id = $1`. The service computes `is_overdue`
from the row and `now` after the query.

Create (transaction, 9), checks in the order of 6.5:

```sql
SELECT total_copies FROM books WHERE id = $book FOR UPDATE            -- none: 404 book
SELECT id FROM members WHERE id = $member FOR UPDATE                  -- none: 404 member
SELECT COUNT(*) FROM loans WHERE book_id = $book AND returned_at IS NULL     -- >= total_copies: 409 no_copies_available
SELECT COUNT(*) FROM loans WHERE member_id = $member AND returned_at IS NULL -- >= 5: 409 loan_limit_reached
INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1,$2,$3,$4) RETURNING id
-- then the one-loan select, commit
```

Return (transaction): `SELECT book_id, member_id FROM loans WHERE id = $1` (none: 404 loan);
`SELECT id FROM books WHERE id = $book FOR UPDATE`; `SELECT id FROM members WHERE id = $member FOR UPDATE`;
`SELECT returned_at FROM loans WHERE id = $1 FOR UPDATE` (gone: 404; not null: 409
`loan_already_returned`); `UPDATE loans SET returned_at = $now WHERE id = $1`; the one-loan
select; commit.

### 8.4 Auth

```sql
SELECT id, username, password_hash FROM users WHERE lower(username) = lower($1)
INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)
SELECT u.id, u.username FROM sessions s JOIN users u ON u.id = s.user_id
  WHERE s.token_hash = $1 AND s.expires_at > $2
DELETE FROM sessions WHERE token_hash = $1
SELECT 1 FROM sessions WHERE expires_at <= $1 LIMIT 1      -- the sweep: look first
DELETE FROM sessions WHERE expires_at <= $1                -- ... and delete only if found
```

The unique index on users is `lower(username)` (`uq_users_username`), "C" collation, so
usernames are unique and matched case-insensitively with ASCII folding only. The admin
user of a box is created with `library-web create-user <name> --password-stdin`
(password at least 8 characters).

## 9. Locking and transactions

Postgres runs at `READ COMMITTED`. Check-then-write sequences take row locks first, in
one fixed order everywhere: **book, then member, then loan**, so they cannot deadlock.
`FOR UPDATE` on the book row means a loan cannot be created between the active-loan
count of an update or delete and the write; the same for the member row on member
delete. Returning early from a transaction rolls it back. Single-statement writes
(member update) need no transaction. Reads take no locks.

## 10. Differences between SQLite and Postgres

Rails only needs the Postgres behaviour above; this list says where the Rust app's two
backends differ, so a reader of the Rust code is not surprised.

- **Search of books:** SQLite uses an FTS5 table `books_fts` (`MATCH '{title author isbn} :
  ("anc"* AND "ada"*) [OR isbn : "digits"*]'`) kept in step by triggers, folds the case of
  all Unicode, and counts a word search from the index alone; Postgres uses the
  `book_search_vector` GIN index and a window total (8.1). The meaning is the same.
- **Substring search** (punctuation-only books `q`, members `q`): `LIKE ... ESCAPE '\'`
  on SQLite (ASCII case-insensitive), `ILIKE` on Postgres (`"C"` collation: ASCII only).
- **Usernames:** SQLite `COLLATE NOCASE` unique column; Postgres a unique index on
  `lower(username)` with `lower($1)` in the lookup. Both fold ASCII only.
- **Unique-violation names:** SQLite reports `books.isbn` / `members.email`; Postgres
  reports the index names `uq_books_isbn` / `uq_members_email`, translated to the same.
  The ISBN index is partial (`WHERE isbn IS NOT NULL`) on SQLite and plain on Postgres
  (NULLs are distinct there anyway).
- **Atomic check-and-write:** SQLite `BEGIN IMMEDIATE` (one writer); Postgres `FOR UPDATE`
  as in 9.
- **Sweep of sessions:** SQLite looks first with the same `SELECT 1` and skips the
  delete; both backends do the same observable thing.
- **Collation of ordering:** both order text bytewise (`BINARY` / `"C"`).
- **Pool:** 10 connections on both (`DATABASE_MAX_CONNECTIONS` overrides).
- **Change log and sync worker** (SQLite node mode only) do not exist on Postgres and are
  not part of the HTTP contract.
