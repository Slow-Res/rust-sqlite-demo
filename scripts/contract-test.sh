#!/usr/bin/env bash
# Contract test of the library API (docs/API-CONTRACT.md) against a running box.
#   scripts/contract-test.sh BASE_URL
# BASE_URL is where the box answers, e.g. http://192.168.48.4 (nginx of a load-test
# box; docs/LOADTEST.md, "Contract test"). The test logs in as the admin account,
# creates its own books, members and loans (all tagged with a random word, removed at
# the end), and checks every route: success shapes and values, each validation error,
# 401, 404, 405, 413, the 409 answers, cursor and offset paging, search, the
# availability filter and the cached totals. Prints one line per failed check; exit 0
# only if every check passed. Needs curl, jq, sha256sum, base64 and GNU date/sed.
#
# Environment (defaults in brackets):
#   LT_ADMIN_USER [admin], LT_ADMIN_PASSWORD [loadtest-local-only]   the librarian to log in as
#   CT_TTL_MS [1000]       the box's LIST_COUNT_TTL_MS: how long a total may lag a write
#   CT_SESSION_TTL_S [604800]  the box's SESSION_TTL_HOURS in seconds (cookie Max-Age)
#   CT_COOKIE_SECURE [false]   the box's COOKIE_SECURE
#   CT_PSQL                a command that runs SQL on the box's Postgres, reading the
#                          statement on stdin and printing bare values, for example
#                          "docker exec -i lt-ct-web-postgres-1 psql -U library -d library -tA".
#                          Needed for the checks that cannot be made through the API
#                          (overdue loans, expired sessions, token hashing); skipped without it.
#   CT_VERBOSE             set to print a line for every passed check too
set -uo pipefail

BASE=${1:-${BASE_URL:-}}
[ -n "$BASE" ] || { echo "usage: scripts/contract-test.sh BASE_URL" >&2; exit 2; }
BASE=${BASE%/}
# LOCAL-ONLY test credentials of the throwaway load-test database (as in scripts/loadtest.sh).
ADMIN_USER=${LT_ADMIN_USER:-admin}
ADMIN_PASSWORD=${LT_ADMIN_PASSWORD:-loadtest-local-only}
TTL_MS=${CT_TTL_MS:-1000}
SESSION_TTL_S=${CT_SESSION_TTL_S:-604800}
COOKIE_SECURE=${CT_COOKIE_SECURE:-false}
PSQL=${CT_PSQL:-}
for tool in curl jq sha256sum base64 date sed; do
  command -v "$tool" >/dev/null || { echo "contract-test: $tool is required" >&2; exit 2; }
done

T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
PASS=0 FAIL=0 SKIPPED=0

# ------------------------------------------------------------------ helpers

pass() {
  PASS=$((PASS + 1))
  if [ -n "${CT_VERBOSE:-}" ]; then echo "ok:   $1"; fi
  return 0
}
fail() {
  FAIL=$((FAIL + 1))
  echo "FAIL: $1${2:+ ($2)}"
}
skip() {
  SKIPPED=$((SKIPPED + 1))
  echo "skip: $1 (set CT_PSQL)" >&2
}

# call METHOD PATH [BODY [TOKEN [curl args...]]]: one request. Sets R_CODE, R_BODY and
# R_HDR (the response headers, CRs removed). A BODY is sent as JSON (or as $CTYPE); a
# TOKEN is sent as the session cookie.
CTYPE=application/json
call() {
  local method=$1 path=$2 body=${3-} tok=${4-}
  shift $(($# < 4 ? $# : 4))
  local args=(-sS --max-time 30 -X "$method" -D "$T/h" -o "$T/b" -w '%{http_code}')
  [ -z "$tok" ] || args+=(-H "Cookie: library_session=$tok")
  [ -z "$body" ] || args+=(-H "Content-Type: $CTYPE" --data-binary "$body")
  R_CODE=$(curl "${args[@]}" "$@" "$BASE$path" 2>"$T/err") || R_CODE="000 $(head -c 200 "$T/err")"
  R_BODY=$(cat "$T/b" 2>/dev/null)
  R_HDR=$(tr -d '\r' <"$T/h" 2>/dev/null)
  : >"$T/b"
  : >"$T/h"
}
# Shorthands: authenticated as the admin session, and not authenticated.
A() { call "$1" "$2" "${3-}" "$TOK" "${@:4}"; }
N() { call "$1" "$2" "${3-}" "" "${@:4}"; }

hdr() { sed -n "s/^$1: *//Ip" <<<"$R_HDR" | head -1; }

status() { # name want
  if [ "$R_CODE" = "$2" ]; then pass "$1"; else fail "$1" "status $R_CODE, want $2: ${R_BODY:0:200}"; fi
}
# jqok NAME FILTER [jq args]: the filter must be true for the response body.
jqok() {
  local name=$1 filter=$2
  shift 2
  if jq -e "$@" "$filter" <<<"$R_BODY" >/dev/null 2>&1; then pass "$name"; else fail "$name" "body: ${R_BODY:0:300}"; fi
}
# ok NAME STATUS FILTER [jq args]: a status and a body filter in one check.
ok() {
  local name=$1 want=$2 filter=$3
  shift 3
  if [ "$R_CODE" != "$want" ]; then fail "$name" "status $R_CODE, want $want: ${R_BODY:0:200}"; return; fi
  jqok "$name" "$filter" "$@"
}
# err NAME STATUS CODE MESSAGE [FIELDS-JSON]: the error envelope (§ Errors). MESSAGE "*"
# accepts any non-empty message (the framework's own text). Without FIELDS the
# `fields` key must be absent.
err() {
  local name=$1 want=$2 code=$3 msg=$4 fields=${5-}
  if [ "$R_CODE" != "$want" ]; then fail "$name" "status $R_CODE, want $want: ${R_BODY:0:200}"; return; fi
  local f='keys == ["error"] and (.error | (keys - ["code","message","fields"]) == []) and .error.code == $c and (.error.message | type == "string" and length > 0)'
  local args=(--arg c "$code")
  if [ "$msg" != "*" ]; then f+=' and .error.message == $m'; args+=(--arg m "$msg"); fi
  if [ -n "$fields" ]; then f+=' and .error.fields == $f'; args+=(--argjson f "$fields"); else f+=' and (.error | has("fields") | not)'; fi
  case "$(hdr content-type)" in application/json*) ;; *) fail "$name" "content-type '$(hdr content-type)'"; return ;; esac
  jqok "$name" "$f" "${args[@]}"
}
# val NAME FIELDS-JSON: a 422 validation_error with exactly these field messages.
val() { err "$1" 422 validation_error "Some fields are invalid" "$2"; }
equal() { # name got want
  if [ "$2" = "$3" ]; then pass "$1"; else fail "$1" "got '$2', want '$3'"; fi
}

# sql STATEMENT: run it on the box's database (CT_PSQL), print the result.
sql() { echo "$1" | $PSQL 2>&1; }

TS_RE='^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$'
BOOK_KEYS='["author","available_copies","created_at","id","isbn","published_year","title","total_copies","updated_at"]'
MEMBER_KEYS='["active_loans","created_at","email","id","name","updated_at"]'
LOAN_KEYS='["book_id","book_title","borrowed_at","due_at","id","is_overdue","member_id","member_name","returned_at"]'
PAGE_KEYS='["items","next_cursor","page","per_page","total"]'
BAD_TOKEN=$(printf 'f%.0s' $(seq 64))

# ------------------------------------------------------------------ test data

TAG="ct$(date +%s)$(printf '%x' $RANDOM)"   # lowercase letters and digits: one search word
MADE_BOOKS=() MADE_MEMBERS=() MADE_LOANS=()

# isbn13 / isbn10: a valid, random ISBN. 979 prefix: the load-test seed uses 978.
isbn13() {
  local body sum=0 i
  body=$(printf '979%09d' $(((RANDOM * 32768 + RANDOM) % 1000000000)))
  for i in {0..11}; do sum=$((sum + ${body:i:1} * (i % 2 == 0 ? 1 : 3))); done
  echo "$body$(((10 - sum % 10) % 10))"
}
isbn10() { # isbn10 [x]: with "x", one whose check digit is X
  local body sum i check
  while :; do
    body=$(printf '%09d' $(((RANDOM * 32768 + RANDOM) % 1000000000)))
    sum=0
    for i in {0..8}; do sum=$((sum + ${body:i:1} * (10 - i))); done
    check=$(((11 - sum % 11) % 11))
    [ "$check" = 10 ] && check=X
    if [ "${1-}" != x ] || [ "$check" = X ]; then echo "$body$check"; return; fi
  done
}

book_body() { # title author isbn year copies  (empty isbn/year = null)
  jq -nc --arg t "$1" --arg a "$2" --arg i "${3-}" --arg y "${4-}" --argjson c "${5:-1}" \
    '{title:$t, author:$a, isbn:(if $i == "" then null else $i end), published_year:(if $y == "" then null else ($y | tonumber) end), total_copies:$c}'
}
member_body() { jq -nc --arg n "$1" --arg e "$2" '{name:$n, email:$e}'; }

# new_book TITLE [AUTHOR [ISBN [YEAR [COPIES]]]]: create and check; sets NB_ID.
new_book() {
  A POST /api/books "$(book_body "$1" "${2:-Ct Author}" "${3-}" "${4-}" "${5:-1}")"
  status "setup: create book '$1'" 201
  NB_ID=$(jq -r '.id // empty' <<<"$R_BODY")
  [ -z "$NB_ID" ] || MADE_BOOKS+=("$NB_ID")
}
new_member() { # NAME EMAIL: sets NM_ID
  A POST /api/members "$(member_body "$1" "$2")"
  status "setup: create member '$1'" 201
  NM_ID=$(jq -r '.id // empty' <<<"$R_BODY")
  [ -z "$NM_ID" ] || MADE_MEMBERS+=("$NM_ID")
}
new_loan() { # BOOK MEMBER: sets NL_ID
  A POST /api/loans "{\"book_id\":$1,\"member_id\":$2}"
  status "setup: loan book $1 to member $2" 201
  NL_ID=$(jq -r '.id // empty' <<<"$R_BODY")
  [ -z "$NL_ID" ] || MADE_LOANS+=("$NL_ID")
}
ids() { jq -c '[.items[].id]' <<<"$R_BODY"; }

# login USER PASSWORD: sets LOGIN_TOKEN from the Set-Cookie header ("" if none).
login() {
  N POST /api/auth/login "$(jq -nc --arg u "$1" --arg p "$2" '{username:$u, password:$p}')"
  LOGIN_TOKEN=$(sed -n 's/^set-cookie: library_session=\([^;]*\).*/\1/Ip' <<<"$R_HDR" | head -1)
}

# decode_cursor CURSOR: the JSON inside (base64url without padding).
decode_cursor() {
  local c=${1//-/+}
  c=${c//_//}
  while [ $((${#c} % 4)) -ne 0 ]; do c+="="; done
  base64 -d <<<"$c" 2>/dev/null | jq -c . 2>/dev/null
}

# wait_total NAME PATH EXPECTED: poll a list until its total is EXPECTED, for at most
# 3 TTLs (a cached total may lag a write by up to the TTL).
wait_total() {
  local name=$1 path=$2 want=$3 deadline=$((SECONDS + 3 * TTL_MS / 1000 + 3)) got=
  while :; do
    A GET "$path"
    got=$(jq -r '.total // empty' <<<"$R_BODY")
    [ "$got" = "$want" ] && { pass "$name"; return; }
    if ((SECONDS >= deadline)); then fail "$name" "total $got, want $want after the TTL"; return; fi
    sleep 0.3
  done
}
total_of() { A GET "$1"; jq -r '.total // empty' <<<"$R_BODY"; }

# ------------------------------------------------------------------ 1. health, headers, envelope

N GET /api/health
ok "GET /api/health" 200 'keys == ["node_id","status","version"] and .status == "ok" and (.node_id | type == "string" and length > 0) and (.version | type == "string" and length > 0)'
equal "health content-type" "$(hdr content-type)" "application/json"
equal "header x-content-type-options" "$(hdr x-content-type-options)" "nosniff"
equal "header x-frame-options" "$(hdr x-frame-options)" "DENY"
equal "header referrer-policy" "$(hdr referrer-policy)" "same-origin"
if [[ "$(hdr x-request-id)" =~ ^[0-9a-f-]{36}$ ]]; then pass "header x-request-id is a UUID"; else fail "header x-request-id is a UUID" "'$(hdr x-request-id)'"; fi
call GET /api/health "" "$BAD_TOKEN"
status "health ignores a bad session cookie" 200

for route in "GET /api/nope" "POST /api/nope/deeper" "DELETE /api/auth/nope" "GET /api/books/" "GET /api/members/" "GET /api" "PUT /api/loans/1" "GET /api/loans/1" "GET /api/health/x"; do
  N "${route% *}" "${route#* }"
  err "unknown route $route is a JSON 404" 404 not_found "route not found"
done
for route in "DELETE /api/health" "POST /api/health" "GET /api/auth/login" "PUT /api/auth/login" "POST /api/auth/me" "GET /api/auth/logout" \
  "PATCH /api/books/1" "POST /api/books/1" "DELETE /api/books" "PUT /api/books" "PATCH /api/members/1" "DELETE /api/members" \
  "GET /api/loans/1/return" "PUT /api/loans/1/return" "DELETE /api/loans" "PUT /api/loans"; do
  N "${route% *}" "${route#* }"
  err "wrong method $route is a JSON 405 (before auth)" 405 method_not_allowed "method not allowed"
done
head -c 70000 /dev/zero | tr '\0' 'a' >"$T/big"
for path in /api/auth/login /api/books; do
  curl -sS --max-time 30 -D "$T/h" -o "$T/b" -w '%{http_code}' -X POST -H 'Content-Type: application/json' --data-binary @"$T/big" "$BASE$path" >"$T/code"
  R_CODE=$(cat "$T/code") R_BODY=$(cat "$T/b") R_HDR=$(tr -d '\r' <"$T/h")
  err "POST $path with a 70000-byte body is a JSON 413" 413 payload_too_large "request body too large"
done

# ------------------------------------------------------------------ 2. auth

login "$ADMIN_USER" "$ADMIN_PASSWORD"
TOK=$LOGIN_TOKEN
ok "login succeeds" 200 'keys == ["id","username"] and (.id | type == "number" and . >= 1 and . == floor) and (.username | type == "string" and length > 0)'
ADMIN_ID=$(jq -r .id <<<"$R_BODY")
[ -n "$TOK" ] || { echo "FAIL: login gave no session cookie; cannot go on"; exit 1; }
equal "login content-type" "$(hdr content-type)" "application/json"
if [[ "$TOK" =~ ^[0-9a-f]{64}$ ]]; then pass "session token is 64 lowercase hex characters"; else fail "session token is 64 lowercase hex characters" "'$TOK'"; fi
cookie_line=$(grep -i '^set-cookie:' <<<"$R_HDR")
[ "$(grep -ic '^set-cookie:' <<<"$R_HDR")" = 1 ] && pass "login sets exactly one cookie" || fail "login sets exactly one cookie" "$cookie_line"
for attr in "HttpOnly" "SameSite=Lax" "Path=/" "Max-Age=$SESSION_TTL_S"; do
  # The attribute as a whole `; `-separated item, any letter case.
  if grep -iq "; *${attr}\(;\|$\)" <<<"$cookie_line"; then pass "cookie attribute $attr"; else fail "cookie attribute $attr" "$cookie_line"; fi
done
if grep -iq "; *Secure\(;\|$\)" <<<"$cookie_line"; then has_secure=true; else has_secure=false; fi
equal "cookie attribute Secure is $COOKIE_SECURE" "$has_secure" "$COOKIE_SECURE"
if grep -iq "domain=" <<<"$cookie_line"; then fail "cookie has no Domain attribute" "$cookie_line"; else pass "cookie has no Domain attribute"; fi

A GET /api/auth/me
ok "GET /api/auth/me" 200 'keys == ["id","username"] and .id == $id' --argjson id "$ADMIN_ID"
login "$(tr a-z A-Z <<<"$ADMIN_USER")  " "$ADMIN_PASSWORD"
ok "login: username is trimmed and case-insensitive; the stored username comes back" 200 '.username == $u' --arg u "$ADMIN_USER"
SECOND_TOK=$LOGIN_TOKEN
[ -n "$SECOND_TOK" ] && [ "$SECOND_TOK" != "$TOK" ] && pass "every login gets its own token" || fail "every login gets its own token"
login "$ADMIN_USER" "$ADMIN_PASSWORD$BAD_TOKEN"
err "login with a wrong password" 401 unauthorized "invalid username or password"
[ -z "$LOGIN_TOKEN" ] && pass "failed login sets no cookie" || fail "failed login sets no cookie"
login "no-such-user-$TAG" "whatever-password"
err "login with an unknown user (same answer as a wrong password)" 401 unauthorized "invalid username or password"
login "" ""
val "login with empty fields" '{"password":"must not be empty","username":"must not be empty"}'
login "   " "x"
val "login with a blank username" '{"username":"must not be empty"}'
login "$ADMIN_USER" ""
val "login with an empty password" '{"password":"must not be empty"}'
N POST /api/auth/login '{"username":"admin"}'
err "login without a password field" 400 bad_request "*"
N POST /api/auth/login '{"username":1,"password":"x"}'
err "login with a number as username" 400 bad_request "*"
N POST /api/auth/login '{bad json'
err "login with malformed JSON" 400 bad_request "*"
N POST /api/auth/login ''
err "login without a body" 400 bad_request "*"
CTYPE=text/plain N POST /api/auth/login '{"username":"a","password":"b"}'
err "login with a non-JSON content type" 400 bad_request "*"
CTYPE=application/json

# 401 on every protected route, with no cookie and with a cookie that is no session;
# before the body is read and before the id is looked up.
for route in "GET /api/auth/me" "POST /api/auth/logout" "POST /api/books {}" "PUT /api/books/1 {}" "PUT /api/books/99999999 {}" "DELETE /api/books/1" \
  "GET /api/members" "GET /api/members/1" "POST /api/members {}" "PUT /api/members/1 {}" "DELETE /api/members/1" \
  "GET /api/loans" "POST /api/loans {}" "POST /api/loans/1/return"; do
  read -r m p b <<<"$route"
  call "$m" "$p" "$b"
  err "no cookie: $m $p is 401" 401 unauthorized "Authentication required"
  call "$m" "$p" "$b" "$BAD_TOKEN"
  err "bad cookie: $m $p is 401" 401 unauthorized "Authentication required"
done
call GET /api/auth/me "" ""  -H "Cookie: library_session="
err "an empty session cookie is 401" 401 unauthorized "Authentication required"
call GET /api/members "" "" -H "Cookie: other=1"
err "a cookie of another name is 401" 401 unauthorized "Authentication required"

# logout: only the session that logs out ends.
call POST /api/auth/logout "" "$SECOND_TOK"
status "logout is 204" 204
equal "logout has an empty body" "$R_BODY" ""
cookie_line=$(grep -i '^set-cookie:' <<<"$R_HDR")
if grep -iq '^set-cookie: library_session=;' <<<"$cookie_line" && grep -iq 'Max-Age=0' <<<"$cookie_line" && grep -iq 'Path=/' <<<"$cookie_line"; then pass "logout clears the cookie (empty value, Path=/, Max-Age=0)"; else fail "logout clears the cookie" "$cookie_line"; fi
call GET /api/auth/me "" "$SECOND_TOK"
err "a logged-out session no longer works" 401 unauthorized "Authentication required"
call POST /api/auth/logout "" "$SECOND_TOK"
err "logging out twice is 401" 401 unauthorized "Authentication required"
A GET /api/auth/me
status "another session is untouched by a logout" 200

# ------------------------------------------------------------------ 3. books: create, show, update, delete

YEAR=$(date -u +%Y)
ISBN_A=$(isbn13)
A POST /api/books "$(book_body "  $TAG Alpha  " "  Ann Author " "$ISBN_A" 1999 3)"
ok "POST /api/books" 201 'keys == '"$BOOK_KEYS"' and (.id | type == "number") and .title == $t and .author == "Ann Author" and .isbn == $i and .published_year == 1999 and .total_copies == 3 and .available_copies == 3 and (.created_at | test($re)) and .created_at == .updated_at' \
  --arg t "$TAG Alpha" --arg i "$ISBN_A" --arg re "$TS_RE"
equal "create answers with the created-at time within a minute of now" "$(($(date -u +%s) - $(date -u -d "$(jq -r .created_at <<<"$R_BODY")" +%s) < 60))" 1
equal "create content-type" "$(hdr content-type)" "application/json"
BOOK_A=$(jq -r .id <<<"$R_BODY") BOOK_A_JSON=$R_BODY
MADE_BOOKS+=("$BOOK_A")
N GET "/api/books/$BOOK_A"
ok "GET /api/books/{id} without a session (reads are public); same JSON as create" 200 '. == $b' --argjson b "$BOOK_A_JSON"
A GET "/api/books/$BOOK_A"
status "GET /api/books/{id} with a session" 200

A POST /api/books "$(book_body "$TAG NoIsbn" "X" "" "" 1)"
ok "create without isbn and year: both null" 201 '.isbn == null and .published_year == null'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "{\"title\":\"$TAG Extra\",\"author\":\"X\",\"isbn\":null,\"published_year\":null,\"total_copies\":1,\"unknown_field\":[1]}"
ok "create ignores unknown fields; null isbn and year are fine" 201 '.isbn == null'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "{\"title\":\"$TAG Minimal\",\"author\":\"X\",\"total_copies\":1}"
ok "create without the isbn and published_year keys" 201 '.isbn == null and .published_year == null'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "$(book_body "$TAG BlankIsbn" "X" "   " "" 1)"
ok "a blank isbn means no isbn" 201 '.isbn == null'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")

ISBN10X=$(isbn10 x)
A POST /api/books "$(book_body "$TAG Ten" "X" "$(tr X x <<<"${ISBN10X:0:1}-${ISBN10X:1:3}-${ISBN10X:4:5}-${ISBN10X:9}")" "" 1)"
ok "isbn-10 with hyphens and a lowercase x is stored as digits and capital X" 201 '.isbn == $i' --arg i "$ISBN10X"
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "$(book_body "$TAG Spaced" "X" "$(sed 's/./& /g;s/ $//' <<<"$(isbn13)")" "" 1)"
ok "isbn-13 with spaces is stored without them" 201 '.isbn | test("^[0-9]{13}$")'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")

A POST /api/books "$(book_body "$TAG Dup" "X" "${ISBN_A:0:3}-${ISBN_A:3}" "" 1)"
err "duplicate isbn (typed with a hyphen) is 409 isbn_taken" 409 isbn_taken "a book with this ISBN already exists"

A POST /api/books "$(book_body "" "" "123" 0 0)"
val "create with every field wrong" '{"author":"must not be empty","isbn":"must be a valid ISBN-10 or ISBN-13","published_year":"must be a valid year","title":"must not be empty","total_copies":"must be between 1 and 1000"}'
A POST /api/books "$(book_body "   " "x" "" "" 1)"
val "a blank title" '{"title":"must not be empty"}'
A POST /api/books "$(book_body "t" "  " "" "" 1)"
val "a blank author" '{"author":"must not be empty"}'
A POST /api/books "$(book_body "$(printf 'a%.0s' $(seq 301))" "x" "" "" 1)"
val "a 301-character title" '{"title":"must be at most 300 characters"}'
A POST /api/books "$(book_body "t" "$(printf 'a%.0s' $(seq 201))" "" "" 1)"
val "a 201-character author" '{"author":"must be at most 200 characters"}'
A POST /api/books "$(book_body "$(printf 'é%.0s' $(seq 300))" "$(printf 'é%.0s' $(seq 200))" "" "" 1)"
ok "300 and 200 characters are accepted (characters, not bytes)" 201 '(.title | length) == 300 and (.author | length) == 200'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "$(book_body "t" "x" "$(sed 's/.$/0/' <<<"$(isbn13)")x" "" 1)"
val "an isbn that is neither 10 nor 13 characters" '{"isbn":"must be a valid ISBN-10 or ISBN-13"}'
bad13=$(isbn13)
bad13="${bad13:0:12}$(((${bad13:12:1} + 1) % 10))"
A POST /api/books "$(book_body "t" "x" "$bad13" "" 1)"
val "an isbn-13 with a wrong check digit" '{"isbn":"must be a valid ISBN-10 or ISBN-13"}'
bad10=$(isbn10)
bad10="${bad10:0:9}$([ "${bad10:9:1}" = 0 ] && echo 1 || echo 0)"
A POST /api/books "$(book_body "t" "x" "$bad10" "" 1)"
val "an isbn-10 with a wrong check digit" '{"isbn":"must be a valid ISBN-10 or ISBN-13"}'
A POST /api/books "$(book_body "t" "x" "97930640615XX" "" 1)"
val "an isbn with letters" '{"isbn":"must be a valid ISBN-10 or ISBN-13"}'
A POST /api/books "$(book_body "t" "x" "" 0 1)"
val "published_year 0" '{"published_year":"must be a valid year"}'
A POST /api/books "$(book_body "t" "x" "" -5 1)"
val "published_year negative" '{"published_year":"must be a valid year"}'
A POST /api/books "$(book_body "t" "x" "" $((YEAR + 2)) 1)"
val "published_year two years ahead" '{"published_year":"must be a valid year"}'
A POST /api/books "$(book_body "$TAG Future" "x" "" $((YEAR + 1)) 1)"
ok "published_year of next year is accepted" 201 '.published_year == $y' --argjson y $((YEAR + 1))
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "$(book_body "$TAG Year1" "x" "" 1 1000)"
ok "published_year 1 and total_copies 1000 are accepted" 201 '.published_year == 1 and .total_copies == 1000 and .available_copies == 1000'
MADE_BOOKS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/books "$(book_body "t" "x" "" "" 1001)"
val "total_copies 1001" '{"total_copies":"must be between 1 and 1000"}'
A POST /api/books "$(book_body "t" "x" "" "" -1)"
val "total_copies negative" '{"total_copies":"must be between 1 and 1000"}'
A POST /api/books '{"title":"t","author":"a"}'
err "create without total_copies" 400 bad_request "*"
A POST /api/books '{"author":"a","total_copies":1}'
err "create without a title" 400 bad_request "*"
A POST /api/books '{"title":null,"author":"a","total_copies":1}'
err "create with a null title" 400 bad_request "*"
A POST /api/books '{"title":"t","author":"a","total_copies":"1"}'
err "total_copies as a string" 400 bad_request "*"
A POST /api/books '{"title":"t","author":"a","total_copies":1.5}'
err "total_copies as a fraction" 400 bad_request "*"
A POST /api/books '{"title":"t","author":"a","total_copies":1,"published_year":99999999999}'
err "published_year beyond 32 bits" 400 bad_request "*"
A POST /api/books '[1]'
err "create with an array body" 400 bad_request "*"
A POST /api/books ''
err "create without a body" 400 bad_request "*"

A GET /api/books/99999999
err "GET unknown book" 404 not_found "book not found"
A GET /api/books/0
err "GET book 0" 404 not_found "book not found"
A GET /api/books/-1
err "GET book -1" 404 not_found "book not found"
N GET /api/books/abc
err "GET book with a non-numeric id" 400 bad_request "*"
N GET /api/books/9223372036854775808
err "GET book with an id beyond 64 bits" 400 bad_request "*"

sleep 1.1
A PUT "/api/books/$BOOK_A" "$(book_body "$TAG Alpha Two" "Bo Author" "" 2001 5)"
ok "PUT /api/books/{id}: all fields replaced, isbn cleared, created_at kept, updated_at later" 200 \
  'keys == '"$BOOK_KEYS"' and .id == $id and .title == $t and .author == "Bo Author" and .isbn == null and .published_year == 2001 and .total_copies == 5 and .available_copies == 5 and .created_at == $c and .updated_at > $c' \
  --argjson id "$BOOK_A" --arg t "$TAG Alpha Two" --arg c "$(jq -r .created_at <<<"$BOOK_A_JSON")"
A GET "/api/books/$BOOK_A"
ok "GET after PUT shows the change" 200 '.title == $t and .isbn == null' --arg t "$TAG Alpha Two"
A PUT "/api/books/$BOOK_A" "$(book_body "$TAG Alpha Two" "Bo Author" "$ISBN_A" 2001 5)"
status "PUT can set the isbn again" 200
A PUT "/api/books/$BOOK_A" "$(book_body "$TAG Alpha Two" "Bo Author" "$ISBN_A" 2001 5)"
status "PUT with the book's own isbn is not a conflict" 200
A POST /api/books "$(book_body "$TAG Other" "x" "" "" 1)"
BOOK_B=$(jq -r .id <<<"$R_BODY")
MADE_BOOKS+=("$BOOK_B")
A PUT "/api/books/$BOOK_B" "$(book_body "$TAG Other" "x" "$ISBN_A" "" 1)"
err "PUT with another book's isbn is 409 isbn_taken" 409 isbn_taken "a book with this ISBN already exists"
A PUT "/api/books/$BOOK_B" "$(book_body "" "x" "" "" 1)"
val "PUT validates" '{"title":"must not be empty"}'
A PUT /api/books/99999999 "$(book_body "t" "x" "" "" 1)"
err "PUT unknown book" 404 not_found "book not found"
A PUT /api/books/99999999 "$(book_body "" "x" "" "" 1)"
val "PUT validates before looking the book up" '{"title":"must not be empty"}'
A PUT "/api/books/abc" "$(book_body "t" "x" "" "" 1)"
err "PUT with a non-numeric id" 400 bad_request "*"

# ------------------------------------------------------------------ 4. members

A POST /api/members "$(member_body "  Mia $TAG  " "  Mia.$TAG@Example.COM ")"
ok "POST /api/members: trimmed name, lowercased trimmed email" 201 \
  'keys == '"$MEMBER_KEYS"' and (.id | type == "number") and .name == $n and .email == $e and .active_loans == 0 and (.created_at | test($re)) and .created_at == .updated_at' \
  --arg n "Mia $TAG" --arg e "mia.$TAG@example.com" --arg re "$TS_RE"
MEM_A=$(jq -r .id <<<"$R_BODY") MEM_A_JSON=$R_BODY
MADE_MEMBERS+=("$MEM_A")
A GET "/api/members/$MEM_A"
ok "GET /api/members/{id}" 200 '. == $m' --argjson m "$MEM_A_JSON"
A POST /api/members "$(member_body "Other $TAG" "MIA.$TAG@example.com")"
err "duplicate email, other letter case, is 409 email_taken" 409 email_taken "a member with this email already exists"
A POST /api/members "$(member_body "" "nope")"
val "create with a blank name and a bad email" '{"email":"must be a valid email address","name":"must not be empty"}'
for bad in plain @example.com a@ a@b a@.com a@example. a@@example.com a@b@example.com "a b@@x.com" ""; do
  A POST /api/members "$(member_body "N" "$bad")"
  val "email '$bad' is invalid" '{"email":"must be a valid email address"}'
done
A POST /api/members "$(member_body "N" "$(printf 'a%.0s' $(seq 243))@example.com")"
val "a 255-character email" '{"email":"must be at most 254 characters"}'
A POST /api/members "$(member_body "$(printf 'n%.0s' $(seq 201))" "n201.$TAG@example.com")"
val "a 201-character name" '{"name":"must be at most 200 characters"}'
A POST /api/members "$(member_body "$(printf 'é%.0s' $(seq 200))" "$(printf 'a%.0s' $(seq 242))@example.com")"
ok "a 200-character name and a 254-character email are accepted" 201 '(.name | length) == 200 and (.email | length) == 254'
MADE_MEMBERS+=("$(jq -r .id <<<"$R_BODY")")
A POST /api/members '{"name":"x"}'
err "create member without an email" 400 bad_request "*"
A POST /api/members '{"name":1,"email":"a@b.co"}'
err "create member with a number as name" 400 bad_request "*"

sleep 1.1
A PUT "/api/members/$MEM_A" "$(member_body "Mia Two $TAG" "Mia.$TAG@EXAMPLE.com")"
ok "PUT /api/members/{id}: own email in other letters is fine; updated_at later" 200 \
  '.name == $n and .email == $e and .created_at == $c and .updated_at > $c' \
  --arg n "Mia Two $TAG" --arg e "mia.$TAG@example.com" --arg c "$(jq -r .created_at <<<"$MEM_A_JSON")"
new_member "Noah $TAG" "noah.$TAG@example.com"
MEM_B=$NM_ID
A PUT "/api/members/$MEM_B" "$(member_body "Noah $TAG" "mia.$TAG@example.com")"
err "PUT with another member's email is 409 email_taken" 409 email_taken "a member with this email already exists"
A PUT "/api/members/$MEM_B" "$(member_body "" "noah.$TAG@example.com")"
val "PUT member validates" '{"name":"must not be empty"}'
A PUT /api/members/99999999 "$(member_body "x" "x@example.com")"
err "PUT unknown member" 404 not_found "member not found"
A PUT /api/members/99999999 "$(member_body "" "x@example.com")"
val "PUT member validates before looking it up" '{"name":"must not be empty"}'
A GET /api/members/99999999
err "GET unknown member" 404 not_found "member not found"
A GET /api/members/abc
err "GET member with a non-numeric id" 400 bad_request "*"
A DELETE /api/members/99999999
err "DELETE unknown member" 404 not_found "member not found"

# ------------------------------------------------------------------ 5. loans

# A fresh book with 2 copies and members to borrow it.
new_book "$TAG Loanable" "Lo Author" "" "" 2
LB=$NB_ID
new_member "Lena $TAG" "lena.$TAG@example.com"
LM1=$NM_ID
new_member "Lars $TAG" "lars.$TAG@example.com"
LM2=$NM_ID
new_member "Lou $TAG" "lou.$TAG@example.com"
LM3=$NM_ID

A POST /api/loans "{\"book_id\":$LB,\"member_id\":$LM1}"
ok "POST /api/loans" 201 'keys == '"$LOAN_KEYS"' and (.id | type == "number") and .book_id == $b and .book_title == $bt and .member_id == $m and .member_name == $mn and (.borrowed_at | test($re)) and (.due_at | test($re)) and .returned_at == null and .is_overdue == false and ((.due_at | fromdate) - (.borrowed_at | fromdate)) == 1209600' \
  --argjson b "$LB" --arg bt "$TAG Loanable" --argjson m "$LM1" --arg mn "Lena $TAG" --arg re "$TS_RE"
L1=$(jq -r .id <<<"$R_BODY") L1_JSON=$R_BODY
MADE_LOANS+=("$L1")
A GET "/api/books/$LB"
ok "a loan takes a copy: available_copies 1 of 2" 200 '.total_copies == 2 and .available_copies == 1'
A GET "/api/members/$LM1"
ok "a loan counts in the member's active_loans" 200 '.active_loans == 1'
new_loan "$LB" "$LM2"
L2=$NL_ID
A POST /api/loans "{\"book_id\":$LB,\"member_id\":$LM3}"
err "no copy left is 409 no_copies_available" 409 no_copies_available "no copies of this book are available"
A GET "/api/books/$LB"
ok "available_copies 0 when every copy is out" 200 '.available_copies == 0'
A PUT "/api/books/$LB" "$(book_body "$TAG Loanable" "Lo Author" "" "" 1)"
err "total_copies below the copies on loan is 409" 409 copies_below_active_loans "total copies cannot be less than the copies currently on loan"
A PUT "/api/books/$LB" "$(book_body "$TAG Loanable" "Lo Author" "" "" 2)"
status "total_copies equal to the copies on loan is fine" 200
A PUT "/api/books/$LB" "$(book_body "$TAG Loanable Renamed" "Lo Author" "" "" 3)"
ok "more copies raise available_copies" 200 '.available_copies == 1'
A GET "/api/loans?member_id=$LM1&status=all"
ok "a loan shows the book's current title" 200 '.items[0].book_title == $t' --arg t "$TAG Loanable Renamed"
A DELETE "/api/books/$LB"
err "delete of a book on loan is 409" 409 book_has_active_loans "book has active loans and cannot be deleted"
A DELETE "/api/members/$LM1"
err "delete of a member with a loan is 409" 409 member_has_active_loans "member has active loans and cannot be deleted"
A POST /api/loans "{\"book_id\":$LB,\"member_id\":$LM1}"
status "the same member may borrow the same book again (a copy is free)" 201
MADE_LOANS+=("$(jq -r .id <<<"$R_BODY")")

A POST /api/loans '{"book_id":0,"member_id":-1}'
val "loan with ids below 1" '{"book_id":"must be a positive id","member_id":"must be a positive id"}'
A POST /api/loans "{\"book_id\":$LB,\"member_id\":0}"
val "loan with member_id 0" '{"member_id":"must be a positive id"}'
A POST /api/loans '{"book_id":99999999,"member_id":99999999}'
err "unknown book and member: the book is reported first" 404 not_found "book not found"
A POST /api/loans "{\"book_id\":$LB,\"member_id\":99999999}"
err "unknown member" 404 not_found "member not found"
A POST /api/loans '{"book_id":1}'
err "loan without member_id" 400 bad_request "*"
A POST /api/loans '{"book_id":"1","member_id":1}'
err "loan with a string id" 400 bad_request "*"

# Return.
sleep 1.1
A POST "/api/loans/$L1/return"
ok "POST /api/loans/{id}/return" 200 'keys == '"$LOAN_KEYS"' and .id == $id and (.returned_at | test($re)) and .returned_at > .borrowed_at and .is_overdue == false and .due_at == $due' \
  --argjson id "$L1" --arg re "$TS_RE" --arg due "$(jq -r .due_at <<<"$L1_JSON")"
A POST "/api/loans/$L1/return"
err "second return is 409 loan_already_returned" 409 loan_already_returned "loan has already been returned"
A POST /api/loans/99999999/return
err "return of an unknown loan" 404 not_found "loan not found"
A POST /api/loans/abc/return
err "return with a non-numeric id" 400 bad_request "*"
A GET "/api/members/$LM1"
ok "a return lowers the member's active_loans (the other loan of the member is still out)" 200 '.active_loans == 1'

# Loan limit of 5 and its order against the copies check.
LIM_BOOKS=()
for i in 1 2 3 4 5 6; do
  new_book "$TAG Limit $i" "Li" "" "" 1
  LIM_BOOKS+=("$NB_ID")
done
new_member "Lim $TAG" "lim.$TAG@example.com"
LIM=$NM_ID
for i in 0 1 2 3 4; do new_loan "${LIM_BOOKS[$i]}" "$LIM"; done
A POST /api/loans "{\"book_id\":${LIM_BOOKS[5]},\"member_id\":$LIM}"
err "a sixth loan is 409 loan_limit_reached" 409 loan_limit_reached "member already has the maximum of 5 active loans"
new_loan "${LIM_BOOKS[5]}" "$LM3"
A POST /api/loans "{\"book_id\":${LIM_BOOKS[5]},\"member_id\":$LIM}"
err "a book with no copy left and a member at the limit: no_copies_available comes first" 409 no_copies_available "no copies of this book are available"
A POST "/api/loans/${MADE_LOANS[-1]}/return"
new_loan "${LIM_BOOKS[5]}" "$LM3"
# Return the first limit loan; the member can borrow again.
A GET "/api/loans?member_id=$LIM&status=active&per_page=100"
LIM_FIRST=$(jq -r '.items[0].id' <<<"$R_BODY")
A POST "/api/loans/$LIM_FIRST/return"
new_loan "${LIM_BOOKS[0]}" "$LIM"

# ------------------------------------------------------------------ 6. loan lists

A GET "/api/loans?member_id=$LIM&status=active&per_page=100"
ok "loans?member_id&status=active: exact total (5), ordered by due_at then id, all unreturned" 200 \
  'keys == '"$PAGE_KEYS"' and .total == 5 and (.items | length) == 5 and ([.items[] | [.due_at, .id]] == ([.items[] | [.due_at, .id]] | sort)) and all(.items[]; .returned_at == null and .member_id == $m and keys == '"$LOAN_KEYS"')' --argjson m "$LIM"
A GET "/api/loans?member_id=$LIM&status=returned"
ok "loans?member_id&status=returned: 1 item" 200 '.total == 1 and (.items | length) == 1 and (.items[0].returned_at | type == "string") and .items[0].is_overdue == false'
A GET "/api/loans?member_id=$LIM&status=all"
ok "loans status=all: 6 loans of the member" 200 '.total == 6'
A GET "/api/loans?member_id=$LIM"
ok "loans default status is active" 200 '.total == 5 and all(.items[]; .returned_at == null)'
A GET "/api/loans?member_id=$LIM&status=overdue"
ok "loans status=overdue: none of a fresh member's loans" 200 '.total == 0 and .items == [] and .next_cursor == null'
A GET "/api/loans?book_id=${LIM_BOOKS[0]}&status=all"
ok "loans?book_id: both loans of the book (one returned, one active)" 200 '.total == 2 and all(.items[]; .book_id == $b)' --argjson b "${LIM_BOOKS[0]}"
A GET "/api/loans?book_id=${LIM_BOOKS[0]}&member_id=$LM3&status=all"
ok "loans?book_id&member_id combine with AND" 200 '.total == 0'
A GET "/api/loans?member_id=99999999"
ok "loans of an unknown member: an empty page, not a 404" 200 '.total == 0 and .items == []'
A GET "/api/loans?status=nope"
err "loans with an unknown status" 400 bad_request "*"
A GET "/api/loans?member_id=abc"
err "loans with a non-numeric member_id" 400 bad_request "*"

# Returned loans are listed newest return first.
new_member "Rita $TAG" "rita.$TAG@example.com"
RM=$NM_ID
new_book "$TAG Returned" "R" "" "" 3
RB=$NB_ID
new_loan "$RB" "$RM"
R1=$NL_ID
new_loan "$RB" "$RM"
R2=$NL_ID
new_loan "$RB" "$RM"
R3=$NL_ID
A POST "/api/loans/$R1/return" >/dev/null
sleep 1.1
A POST "/api/loans/$R2/return" >/dev/null
A POST "/api/loans/$R3/return" >/dev/null
A GET "/api/loans?member_id=$RM&status=returned"
ok "loans status=returned: newest return first, then the larger id" 200 '.total == 3 and all(.items[]; .returned_at != null and .is_overdue == false)'
equal "returned loans order" "$(ids)" "[$R3,$R2,$R1]"
A GET "/api/loans?member_id=$RM&status=returned&per_page=1"
RCUR=$(jq -r .next_cursor <<<"$R_BODY")
equal "returned loans cursor is [returned_at in Unix seconds, id]" "$(decode_cursor "$RCUR")" "$(jq -c '.items[-1] | [(.returned_at | fromdate), .id]' <<<"$R_BODY")"
A GET "/api/loans?member_id=$RM&status=returned&per_page=1&after=$RCUR"
ok "returned loans cursor goes to older returns" 200 '.total == 3 and (.items | length) == 1 and .next_cursor != null'
equal "returned loans cursor page item" "$(ids)" "[$R2]"
A GET "/api/loans?member_id=$RM&status=active"
ok "a member whose loans are all returned has no active loans" 200 '.total == 0'

# ------------------------------------------------------------------ 7. paging (cursor and offset) over books

# Five books of one tag, one more with the title of the second: the order is title, then id.
PG=pg$TAG
new_book "$PG b-two" "P" "" "" 1
P2=$NB_ID
new_book "$PG a-one" "P" "" "" 1
P1=$NB_ID
new_book "$PG b-two" "P" "" "" 1
P3=$NB_ID
new_book "$PG c-three" "P" "" "" 1
P4=$NB_ID
new_book "$PG d-four" "P" "" "" 1
P5=$NB_ID
new_book "$PG B-bytewise" "P" "" "" 1
P0=$NB_ID
WANT="[$P0,$P1,$P2,$P3,$P4,$P5]"   # capital B sorts before lowercase a (bytewise), equal titles by id
A GET "/api/books?q=$PG&per_page=100"
ok "books?q: all six, ordered by title (bytewise) then id; the page envelope" 200 \
  'keys == '"$PAGE_KEYS"' and .page == 1 and .per_page == 100 and .total == 6 and .next_cursor == null and .items == (.items | sort_by([.title, .id]))'
equal "books order: title bytewise, then id" "$(ids)" "$WANT"

# Offset paging.
A GET "/api/books?q=$PG&per_page=2"
ok "offset page 1: 2 items, page and per_page echoed, total, a cursor" 200 '.page == 1 and .per_page == 2 and .total == 6 and (.items | length) == 2 and (.next_cursor | type == "string")'
equal "offset page 1 items" "$(ids)" "[$P0,$P1]"
CUR1=$(jq -r .next_cursor <<<"$R_BODY")
A GET "/api/books?q=$PG&per_page=2&page=2"
equal "offset page 2 items" "$(ids)" "[$P2,$P3]"
jqok "offset page 2 echoes page and has a cursor" '.page == 2 and (.next_cursor | type == "string")'
A GET "/api/books?q=$PG&per_page=2&page=3"
equal "offset page 3 items" "$(ids)" "[$P4,$P5]"
jqok "the last offset page has no cursor (exactly full, nothing after)" '.page == 3 and .next_cursor == null and .total == 6'
A GET "/api/books?q=$PG&per_page=4&page=2"
equal "offset page of a partial last page" "$(ids)" "[$P4,$P5]"
A GET "/api/books?q=$PG&per_page=5&page=1"
jqok "a page one short of the total has a cursor" '.next_cursor != null and (.items | length) == 5'
A GET "/api/books?q=$PG&per_page=6"
jqok "a page that is exactly the total has no cursor" '.next_cursor == null and (.items | length) == 6'
A GET "/api/books?q=$PG&per_page=2&page=99"
ok "a page past the end: no items, exact total, no cursor" 200 '.items == [] and .total == 6 and .next_cursor == null and .page == 99'
A GET "/api/books?q=$PG&page=0&per_page=0"
ok "page 0 and per_page 0 are clamped to 1 and 1" 200 '.page == 1 and .per_page == 1 and (.items | length) == 1'
A GET "/api/books?q=$PG&per_page=5000"
ok "per_page is capped at 100" 200 '.per_page == 100'
A GET "/api/books"
ok "default per_page is 20 and page 1" 200 '.per_page == 20 and .page == 1 and (.items | length) == 20'
A GET "/api/books?page=-1"
err "negative page" 400 bad_request "*"
A GET "/api/books?per_page=abc"
err "non-numeric per_page" 400 bad_request "*"
A GET "/api/books?page=99999999999"
err "page beyond 32 bits" 400 bad_request "*"
A GET "/api/books?page=4294967295&per_page=100"
ok "the largest page number does not overflow the offset" 200 '.items == []'

# Cursor paging.
equal "cursor is base64url of [title, id] of the last item" "$(decode_cursor "$CUR1")" "[\"$PG a-one\",$P1]"
case "$CUR1" in *[+/=]*) fail "cursor uses the URL-safe alphabet without padding" "$CUR1" ;; *) pass "cursor uses the URL-safe alphabet without padding" ;; esac
A GET "/api/books?q=$PG&per_page=2&after=$CUR1"
equal "cursor page 2 items" "$(ids)" "[$P2,$P3]"
jqok "cursor page keeps the exact total, page defaults to 1" '.total == 6 and .page == 1 and (.next_cursor | type == "string")'
CUR2=$(jq -r .next_cursor <<<"$R_BODY")
A GET "/api/books?q=$PG&per_page=2&after=$CUR2"
equal "cursor page 3 items" "$(ids)" "[$P4,$P5]"
jqok "the last cursor page has no cursor" '.next_cursor == null'
A GET "/api/books?q=$PG&per_page=2&page=7&after=$CUR1"
equal "with after, page is ignored for the rows" "$(ids)" "[$P2,$P3]"
jqok "with after, page is echoed" '.page == 7'
A GET "/api/books?q=$PG&per_page=3&after=$(printf '%s' "[\"$PG b-two\",$P2]" | base64 -w0 | tr '+/' '-_' | tr -d =)"
equal "a cursor of a tie on the title continues with the larger id" "$(ids)" "[$P3,$P4,$P5]"
A GET "/api/books?q=$PG&per_page=2&after=$(printf '%s' "[\"$PG zzz\",1]" | base64 -w0 | tr '+/' '-_' | tr -d =)"
ok "a cursor past the last item: empty page, exact total, no cursor" 200 '.items == [] and .total == 6 and .next_cursor == null'
# Walking by cursor and by page visits the same rows.
walk=() after=
for _ in 1 2 3 4 5 6 7; do
  A GET "/api/books?q=$PG&per_page=4${after:+&after=$after}"
  walk+=("$(jq -c '[.items[].id]' <<<"$R_BODY")")
  after=$(jq -r '.next_cursor // empty' <<<"$R_BODY")
  [ -n "$after" ] || break
done
equal "walking the cursor in pages of 4" "$(jq -sc 'add' <<<"${walk[*]}")" "$WANT"
equal "the walk takes 2 requests" "${#walk[@]}" 2
for bad in "" "!!!" "e30" "WyJhIl0" "WyJhIiwiYiJd" "$(printf '%s' '[1,2]' | base64 -w0 | tr '+/' '-_' | tr -d =)" "$(printf '%s' '["a","b"]' | base64 -w0 | tr '+/' '-_' | tr -d =)"; do
  A GET "/api/books?after=$bad"
  err "books with the bad cursor '$bad'" 400 bad_request "invalid cursor"
done
# Cursors of other lists have another key type and are refused.
A GET "/api/loans?member_id=$LIM&status=all&per_page=1"
LCUR=$(jq -r .next_cursor <<<"$R_BODY")
A GET "/api/books?after=$LCUR"
err "a loan cursor given to books is invalid" 400 bad_request "invalid cursor"
A GET "/api/members?after=$LCUR"
err "a loan cursor given to members is invalid" 400 bad_request "invalid cursor"
A GET "/api/books?per_page=1"
A GET "/api/loans?after=$(jq -r .next_cursor <<<"$R_BODY")"
err "a book cursor given to loans is invalid" 400 bad_request "invalid cursor"

# Cursors of members and loans.
A GET "/api/members?q=$TAG&per_page=2"
MCUR=$(jq -r .next_cursor <<<"$R_BODY")
MLAST=$(jq -c '[.items[-1].name, .items[-1].id]' <<<"$R_BODY")
equal "members cursor is base64url of [name, id]" "$(decode_cursor "$MCUR")" "$MLAST"
jqok "members?q: exact total, ordered by name then id" '.total >= 5 and .items == (.items | sort_by([.name, .id])) and .page == 1 and .per_page == 2'
A GET "/api/members?q=$TAG&per_page=2&after=$MCUR"
jqok "members cursor page continues after the cursor" '.items[0] as $f | ($f.name > $n or ($f.name == $n and $f.id > $i))' --arg n "$(jq -r '.[0]' <<<"$MLAST")" --argjson i "$(jq -r '.[1]' <<<"$MLAST")"
A GET "/api/loans?member_id=$LIM&status=active&per_page=2"
LCUR=$(jq -r .next_cursor <<<"$R_BODY")
LLAST=$(jq -c '.items[-1] | [(.due_at | fromdate), .id]' <<<"$R_BODY")
equal "active loans cursor is base64url of [due_at in Unix seconds, id]" "$(decode_cursor "$LCUR")" "$LLAST"
A GET "/api/loans?member_id=$LIM&status=active&per_page=2&after=$LCUR"
jqok "active loans cursor continues after the cursor" '.items[0] as $f | ($f.due_at | fromdate) as $d | ($d > $k or ($d == $k and $f.id > $i)) and .total == 5' --argjson k "$(jq '.[0]' <<<"$LLAST")" --argjson i "$(jq '.[1]' <<<"$LLAST")"
equal "loans paged by cursor and by page agree" "$(
  A GET "/api/loans?member_id=$LIM&status=all&per_page=100"
  jq -c '[.items[].id]' <<<"$R_BODY"
)" "$(
  walk=() after=
  for _ in 1 2 3 4 5 6 7 8; do
    A GET "/api/loans?member_id=$LIM&status=all&per_page=2${after:+&after=$after}"
    walk+=("$(jq -c '[.items[].id]' <<<"$R_BODY")")
    after=$(jq -r '.next_cursor // empty' <<<"$R_BODY")
    [ -n "$after" ] || break
  done
  jq -sc 'add' <<<"${walk[*]}"
)"

# ------------------------------------------------------------------ 8. search

S=sx$TAG   # a word of its own
ISBN_S=$(isbn13)
new_book "Quillfeather $S Saga" "Zoë-Ann O'Brien" "$ISBN_S" "" 1
SB1=$NB_ID
new_book "The $S Atlas" "J.R.R. Quillson" "" "" 1
SB2=$NB_ID
new_book "Other $S words" "Nobody" "" "" 1
SB3=$NB_ID
new_book "Dots $S ## weird % chars" "Nobody" "" "" 1
SB4=$NB_ID
A GET "/api/books?q=$S"
ok "books?q=word: every book with that word; total is exact" 200 '.total == 4 and (.items | length) == 4'
A GET "/api/books?q=$(tr a-z A-Z <<<"$S")"
jqok "search ignores letter case" '.total == 4'
A GET "/api/books?q=quill"
jqok "a prefix of a word matches (title word and author word)" '[.items[].id] | (index('"$SB1"') != null)'
A GET "/api/books?q=quillfeath%20$S"
equal "every word is a prefix of some word: AND" "$(ids)" "[$SB1]"
A GET "/api/books?q=$S%20quills"
equal "words may match different columns (title word, author word)" "$(ids)" "[$SB2]"
A GET "/api/books?q=eather%20$S"
jqok "a word is matched from its start, not inside" '.total == 0 and .items == []'
A GET "/api/books?q=$S%20nobody"
jqok "a word of the author matches" '.total == 2'
A GET "/api/books?q=$S%20obrien"
jqok "punctuation separates words (O'Brien is 'o' and 'brien'; 'obrien' matches nothing)" '.total == 0'
A GET "/api/books?q=brien%20$S"
equal "an apostrophe splits an author word" "$(ids)" "[$SB1]"
A GET "/api/books?q=$ISBN_S"
equal "the whole isbn finds the book" "$(ids)" "[$SB1]"
A GET "/api/books?q=${ISBN_S:0:8}"
jqok "an isbn prefix finds the book" '[.items[].id] | index('"$SB1"') != null'
A GET "/api/books?q=${ISBN_S:0:3}-${ISBN_S:3:1}-${ISBN_S:4:4}-${ISBN_S:8:5}"
equal "an isbn typed with hyphens finds the book" "$(ids)" "[$SB1]"
A GET "/api/books?q=${ISBN_S:0:3}%20${ISBN_S:3:6}"
jqok "an isbn typed with a space finds the book" '[.items[].id] | index('"$SB1"') != null'
A GET "/api/books?q=%23%23"
jqok "a query of punctuation only is a substring match on title, author and isbn" '[.items[].id] | index('"$SB4"') != null'
A GET "/api/books?q=%25"
jqok "a bare percent sign matches literally, not as a wildcard" '.total >= 1 and all(.items[]; (.title + .author + (.isbn // "")) | contains("%"))'
A GET "/api/books?q=%22$S%22"
jqok "quotes are not search syntax" '.total == 4'
for q in "foo*" "a:b" "(x" "x)" "a&b" "a|b" "!x" "'" "\\" "-" "o'reilly" "x%20-y" "<->" "a:*"; do
  A GET "/api/books?q=$(jq -rn --arg q "$q" '$q|@uri')"
  status "q=$q is plain text, not an error" 200
done
A GET "/api/books?q=%20%20"
ok "a blank q is no filter" 200 '.total >= 20'
A GET "/api/books?q=$S&availability=available"
jqok "q with availability=available" '.total == 4'
A GET "/api/books?q=$S&per_page=2&page=2"
jqok "a search paged by offset keeps the total of the whole search" '.total == 4 and (.items | length) == 2 and .next_cursor == null'
A GET "/api/books?q=$S&per_page=2"
SCUR=$(jq -r .next_cursor <<<"$R_BODY")
A GET "/api/books?q=$S&per_page=2&after=$SCUR"
jqok "a search paged by cursor keeps the total of the whole search" '.total == 4 and (.items | length) == 2'
A GET "/api/books?q=$(printf 'a%.0s' $(seq 201))"
val "q of 201 characters" '{"q":"must be at most 200 characters"}'
A GET "/api/books?q=$(printf 'a%.0s' $(seq 200))"
status "q of 200 characters" 200
A GET "/api/books?q=$(printf '%%C3%%A9%.0s' $(seq 200))"
status "q of 200 multi-byte characters (characters, not bytes)" 200
A GET "/api/books?q=$(printf '%%C3%%A9%.0s' $(seq 201))"
val "q of 201 multi-byte characters" '{"q":"must be at most 200 characters"}'
N GET "/api/books?q=$S"
status "book search needs no session" 200
A GET "/api/members?q=$(printf 'a%.0s' $(seq 201))"
val "members q of 201 characters" '{"q":"must be at most 200 characters"}'

# Members: a substring of the name or the email, ASCII case folded, % and _ literal.
new_member "Per%cent $TAG" "pc.$TAG@example.com"
new_member "UnderXscore $TAG" "ux.$TAG@example.com"
new_member "Under_score $TAG" "us.$TAG@example.com"
UND=$NM_ID
A GET "/api/members?q=r_score%20$TAG"
ok "members q is a substring of the name (spaces included); _ is literal, UnderXscore does not match" 200 '.total == 1 and .items[0].id == $id' --argjson id "$UND"
A GET "/api/members?q=r_s"
jqok "an underscore matches literally" 'all(.items[]; (.name + .email) | ascii_downcase | contains("r_s")) and ([.items[].id] | index('"$UND"') != null)'
A GET "/api/members?q=$(tr a-z A-Z <<<"lena%20$TAG")"
jqok "members q ignores case" '.total == 1 and .items[0].name == $n' --arg n "Lena $TAG"
A GET "/api/members?q=LARS.$TAG@"
jqok "members q matches inside the email" '.total == 1'
A GET "/api/members?q=%25"
jqok "a percent sign matches literally in members" 'all(.items[]; (.name + .email) | contains("%")) and .total >= 1'
A GET "/api/members?q=ena%20$TAG"
jqok "members q is a substring match, not a word prefix" '.total == 1'
A GET "/api/members?q=$TAG&per_page=100"
jqok "members?q total is exact and counts every match" '.total == (.items | length)'

# ------------------------------------------------------------------ 9. availability filter

AV=av$TAG
new_book "$AV free" "Av" "" "" 2
AV_FREE=$NB_ID
new_book "$AV part" "Av" "" "" 2
AV_PART=$NB_ID
new_book "$AV full" "Av" "" "" 1
AV_FULL=$NB_ID
new_member "Ava $TAG" "ava.$TAG@example.com"
AVM=$NM_ID
new_loan "$AV_PART" "$AVM"
new_loan "$AV_FULL" "$AVM"
A GET "/api/books?q=$AV&availability=all"
equal "availability=all, books of the tag" "$(ids)" "[$AV_FREE,$AV_FULL,$AV_PART]"
A GET "/api/books?q=$AV&availability=available"
equal "availability=available: free and partly loaned" "$(ids)" "[$AV_FREE,$AV_PART]"
jqok "available: total is exact with q, and every book has a free copy" '.total == 2 and all(.items[]; .available_copies > 0)'
A GET "/api/books?q=$AV&availability=on_loan"
equal "availability=on_loan: partly loaned and full" "$(ids)" "[$AV_FULL,$AV_PART]"
jqok "on_loan: total is exact with q, and every book has a loan out" '.total == 2 and all(.items[]; .available_copies < .total_copies)'
A GET "/api/books?q=$AV&availability=on_loan&per_page=1"
jqok "availability and search paged: total of the whole filter, a cursor" '.total == 2 and (.next_cursor | type == "string")'
A GET "/api/books?q=$AV&availability=on_loan&per_page=1&after=$(jq -r .next_cursor <<<"$R_BODY")"
equal "availability and search paged by cursor" "$(ids)" "[$AV_PART]"
A GET "/api/books?availability=available&per_page=100"
jqok "availability=available without q: every item has a free copy" 'all(.items[]; .available_copies > 0)'
A GET "/api/books?availability=on_loan&per_page=100"
jqok "availability=on_loan without q: every item has a loan out" 'all(.items[]; .available_copies < .total_copies) and (.items | length) >= 1'
A GET "/api/books?availability=nope"
err "unknown availability" 400 bad_request "*"
A GET "/api/books?availability="
err "empty availability" 400 bad_request "*"
A GET "/api/books?availability=on_loan&q=%20&per_page=1"
jqok "a blank q with availability is the availability filter alone" '.total >= 1'

# ------------------------------------------------------------------ 10. totals and their cache

# Unfiltered totals may lag a write by the TTL. Let the caches expire, read every
# unfiltered total, write, and wait for each total to catch up with exactly the writes.
sleep "$(awk -v ms="$TTL_MS" 'BEGIN { printf "%.1f", ms / 1000 + 0.6 }')"
t_books=$(total_of "/api/books")
t_avail=$(total_of "/api/books?availability=available")
t_onloan=$(total_of "/api/books?availability=on_loan")
t_members=$(total_of "/api/members")
t_active=$(total_of "/api/loans")
t_returned=$(total_of "/api/loans?status=returned")
t_all=$(total_of "/api/loans?status=all")
# Writes: +2 books (one fully loaned: 1 on loan), +1 member, 1 loan, 1 loan returned earlier on.
new_book "$TAG TotalsA" "T" "" "" 1
TA=$NB_ID
new_book "$TAG TotalsB" "T" "" "" 2
TB=$NB_ID
new_member "Tom $TAG" "tom.$TAG@example.com"
TM=$NM_ID
new_loan "$TA" "$TM"
TL1=$NL_ID
new_loan "$TB" "$TM"
TL2=$NL_ID
A POST "/api/loans/$TL2/return"
status "setup: return a loan" 200
# books +2; available: A is full (-0, it was never counted), B is free: +1; on_loan +1 (A);
# members +1; active loans +1 (TL1); returned +1 (TL2); all loans +2.
wait_total "books total follows the writes (+2)" "/api/books" $((t_books + 2))
wait_total "available books total follows the writes (+1)" "/api/books?availability=available" $((t_avail + 1))
wait_total "on_loan books total follows the writes (+1)" "/api/books?availability=on_loan" $((t_onloan + 1))
wait_total "members total follows the writes (+1)" "/api/members" $((t_members + 1))
wait_total "active loans total follows the writes (+1)" "/api/loans" $((t_active + 1))
wait_total "returned loans total follows the writes (+1)" "/api/loans?status=returned" $((t_returned + 1))
wait_total "all loans total follows the writes (+2)" "/api/loans?status=all" $((t_all + 2))
# Filtered totals are exact at once.
A GET "/api/loans?member_id=$TM&status=all"
jqok "loans?member_id total is exact right after the write" '.total == 2'
A DELETE "/api/books/$TB"
status "delete a book whose only loan is returned (the loan goes with it)" 204
A GET "/api/loans?book_id=$TB&status=all"
jqok "the returned loan of a deleted book is gone" '.total == 0'
A GET "/api/books/$TB"
err "a deleted book is 404" 404 not_found "book not found"
A DELETE "/api/books/$TB"
err "deleting a book twice is 404" 404 not_found "book not found"
for i in "${!MADE_BOOKS[@]}"; do [ "${MADE_BOOKS[$i]}" != "$TB" ] || MADE_BOOKS[$i]=; done
# The total of an unfiltered list drops at most one TTL after a delete.
wait_total "books total follows a delete (+1 net)" "/api/books" $((t_books + 1))

# ------------------------------------------------------------------ 11. the database (CT_PSQL)

if [ -z "$PSQL" ]; then
  skip "overdue loans, expired sessions, session sweep, token hashing"
else
  # Overdue: only a loan whose due_at is in the past (unreturned) is overdue.
  new_member "Olga $TAG" "olga.$TAG@example.com"
  OM=$NM_ID
  new_book "$TAG Overdue" "O" "" "" 3
  OB=$NB_ID
  new_loan "$OB" "$OM"
  O1=$NL_ID
  new_loan "$OB" "$OM"
  O2=$NL_ID
  new_loan "$OB" "$OM"
  O3=$NL_ID
  A POST "/api/loans/$O3/return"
  sql "UPDATE loans SET due_at = $(date -u +%s) - 86400 WHERE id IN ($O1, $O3)" >/dev/null
  A GET "/api/loans?member_id=$OM&status=overdue"
  ok "status=overdue: the unreturned loan past its due date, exact right away" 200 '.total == 1 and .items[0].id == $id and .items[0].is_overdue == true' --argjson id "$O1"
  A GET "/api/loans?member_id=$OM&status=active"
  ok "status=active includes the overdue loan, flagged" 200 '.total == 2 and ([.items[] | select(.id == $id)][0].is_overdue == true) and ([.items[] | select(.id == $o2)][0].is_overdue == false)' --argjson id "$O1" --argjson o2 "$O2"
  A GET "/api/loans?member_id=$OM&status=all"
  ok "a returned loan is never overdue, even past its due date" 200 '([.items[] | select(.id == $r)][0].is_overdue == false)' --argjson r "$O3"
  A GET "/api/loans?status=overdue&per_page=100"
  jqok "status=overdue lists only overdue loans, ordered by due_at then id" 'all(.items[]; .is_overdue == true and .returned_at == null) and (.items | map([.due_at, .id])) == (.items | map([.due_at, .id]) | sort)'
  A GET "/api/loans?member_id=$OM&status=overdue&per_page=1"
  ok "an overdue page has a cursor only when more follow" 200 '.next_cursor == null'

  # Sessions: the token is stored hashed, expires by expires_at, logout deletes it, login sweeps.
  login "$ADMIN_USER" "$ADMIN_PASSWORD"
  ST=$LOGIN_TOKEN
  hash=$(printf '%s' "$ST" | sha256sum | cut -d' ' -f1)
  equal "the session row holds SHA-256 hex of the token" "$(sql "SELECT count(*) FROM sessions WHERE token_hash = '$hash'")" 1
  equal "the raw token is not stored" "$(sql "SELECT count(*) FROM sessions WHERE token_hash = '$ST'")" 0
  equal "the session lasts the cookie's Max-Age" "$(sql "SELECT expires_at - created_at FROM sessions WHERE token_hash = '$hash'")" "$SESSION_TTL_S"
  sql "UPDATE sessions SET expires_at = $(date -u +%s) - 10 WHERE token_hash = '$hash'" >/dev/null
  call GET /api/auth/me "" "$ST"
  err "an expired session is 401" 401 unauthorized "Authentication required"
  equal "an expired session row stays until the next login" "$(sql "SELECT count(*) FROM sessions WHERE token_hash = '$hash'")" 1
  login "$ADMIN_USER" "$ADMIN_PASSWORD"
  equal "a login sweeps expired sessions" "$(sql "SELECT count(*) FROM sessions WHERE token_hash = '$hash'")" 0
  ST2=$LOGIN_TOKEN
  hash2=$(printf '%s' "$ST2" | sha256sum | cut -d' ' -f1)
  call POST /api/auth/logout "" "$ST2"
  equal "logout deletes the session row" "$(sql "SELECT count(*) FROM sessions WHERE token_hash = '$hash2'")" 0
  # The password hash is Argon2id.
  equal "the admin's password is an Argon2id PHC string" "$(sql "SELECT left(password_hash, 10) FROM users WHERE lower(username) = lower('$ADMIN_USER')")" '$argon2id$'
fi

# ------------------------------------------------------------------ cleanup

# Return what is still out, then delete what the test made (loans go with their book or member).
for l in "${MADE_LOANS[@]}"; do A POST "/api/loans/$l/return" >/dev/null; done
for i in "${!MADE_BOOKS[@]}"; do [ -z "${MADE_BOOKS[$i]}" ] || A DELETE "/api/books/${MADE_BOOKS[$i]}" >/dev/null; done
for m in "${MADE_MEMBERS[@]}"; do A DELETE "/api/members/$m" >/dev/null; done
# Anything the test left behind (a failed step): list by tag and say so.
A GET "/api/books?q=$TAG&per_page=1"
left=$(jq -r '.total // 0' <<<"$R_BODY")
[ "$left" = 0 ] || echo "note: $left books of this run ($TAG) are still there" >&2
call POST /api/auth/logout "" "$TOK"
status "logout of the test session" 204

echo "contract-test: $((PASS + FAIL)) checks, $FAIL failed${SKIPPED:+, $SKIPPED skipped}" >&2
[ "$FAIL" = 0 ]
