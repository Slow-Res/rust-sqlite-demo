# Ramp mode: steps.jsonl (slurped, final values) into {result, ok, md}: the result
# line, whether a step passed, and summary.md. Args: db, node_cpus, k6_cpus,
# nginx_mem, web_mem, pg_mem, pg_shared, pg_cache, start, step_users, step_s,
# max_users, p95, p99, err, stop (null, or {step, to_sigterm_s, to_exit_s}) and
# live_fail (the step the live reading failed, or null), paging (cursor|offset), mix (nosearch|search),
# db_pool (the app's pool size, or "" for its default), pg_work_mem, and clock and cool
# (the clock lock and the cool-down gate of the run, JSON of scripts/loadtest.sh, or null)
# and warmup (the warm-up object of ramp-warmup.jq, or null when WARMUP_S is 0).
include "ramp-lib";
include "run-header";
def unit(v; u): if v == null then "n/a" else "\(v) \(u)" end;
def why: "p95 \(unit(.p95; "ms")) / p99 \(if .p99 != null then unit(.p99; "ms") else "over the limit" end) / err \(.error_pct)%";

. as $steps
| ([$steps[] | select(.pass | not)] | first) as $ff
| (if $ff == null then ($steps | last) else ([$steps[] | select(.step < $ff.step)] | last) end) as $hp
| (if $ff != null and $hp != null then
     "ramp result: highest passing step \($hp.users) users (≈ \($hp.req_per_s) req/s); first failing step \($ff.users) users (\($ff | why))"
   elif $ff != null then
     "ramp result: no passing step; first failing step \($ff.users) users (\($ff | why))"
   else
     "ramp result: highest passing step \($hp.users) users (≈ \($hp.req_per_s) req/s); no failing step up to \($max_users) users"
       + (if $live_fail != null then "; the live reading of step \($live_fail) failed but its final values pass" else "" end)
   end) as $result_base
# The steps the result rests on: the first failing one and the highest passing one.
| ([$ff, $hp] | map(select(. != null))) as $key
| (if any($key[]; .clock_ok == false) then
     " (WARNING: the clock was not steady in "
       + ([$ff | select(. != null and .clock_ok == false) | "the failing step (\(.clock))"] + [$hp | select(. != null and .clock_ok == false) | "the highest passing step (\(.clock))"] | join(" and "))
       + ", so the result may not be reproducible)"
   elif ($key | length) > 0 and all($key[]; .clock_ok == true) then " (clock: ok)"
   else " (clock: not verified)" end) as $clock_note
| ($result_base + $clock_note) as $result
| {
    result: $result,
    ok: ($hp != null),
    md: ("# Ramp test summary\n\n"
      + "Box: nginx + Rust + \(if $db == "postgres" then "Postgres" else "SQLite" end), 2 CPUs (cores \($node_cpus)), 4096 MiB (nginx \($nginx_mem), web \($web_mem)\(if $db == "postgres" then ", postgres \($pg_mem); shared_buffers \($pg_shared), effective_cache_size \($pg_cache)" else "" end)). k6 on cores \($k6_cpus). Browsing: \($paging) paging. Mix: \($mix). Pool: \(if $db_pool == "" then "default (10)" else $db_pool end) connections\(if $db == "postgres" then ", Postgres work_mem \($pg_work_mem)" else "" end).\n"
      + "Ramp: \($start) users, then +\($step_users) every \($step_s) s (a 10 s ramp up, then a hold) up to \($max_users) users, until a step fails. Each user logs in once and keeps the session (no logout; a 401 means a re-login, counted per step). Same action mix and think times as the users model.\n"
      + (if $warmup != null then "warm-up: \($warmup.warmup_s) s (unjudged): the users sign in over the first \($warmup.warmup_s / 2 | floor) s and hold \($start) users until the stairs start; its requests belong to no step and are never PASS or FAIL. Step 0 starts after it.\n" else "" end)
      + "Each step's numbers cover the requests that started in that step, not the whole run. Pass rule per step: p95 < \($p95) ms, p99 < \($p99) ms, errors < \($err)%. Restarts and OOM kills are recorded but are not part of the rule. CPU is of 200% (2 cores); frequency is the current frequency of the box cores sampled every 5 s. The clock column is not part of the rule either: ok = steady (clock locked and the step's average at least 95% of it, no throttling), THROTTLED = a box core or the package throttled during the step, drift N = locked but ran at N MHz on average, - = unlocked and not throttled, so not known.\n"
      + run_header($clock; $cool) + "\n"
      + (if $stop != null then "Stopped early: step \($stop.step) failed; k6 got SIGTERM \($stop.to_sigterm_s) s after that step ended and exited \($stop.to_exit_s) s after it. Numbers are from k6's end-of-test summary.\n\n" else "" end)
      + (if $warmup != null then ($warmup | warmup_line) + "\n\n" else "" end)
      + header + "\n" + ($steps | map(row) | join("\n"))
      + "\n\n**" + $result + "**\n")
  }
