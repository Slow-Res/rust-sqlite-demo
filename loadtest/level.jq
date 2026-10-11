# One level's result. Inputs (--arg/--argjson/--slurpfile): k6 (summary), samples
# (JSON lines of scripts/loadtest.sh), kind, value, ramp, hold, start_s (epoch
# seconds when k6's scenario started), drain, the p95/p99/err/drop limits,
# mode (web|node), ncont (node or box containers), nginx_log (access-log line counts, web mode), setup (the LT_SETUP_DONE data), events (docker die/oom events), before/after (container restart state), rows_before, rows_after.
# Everything is computed from phase:hold only.
def r1: if . == null then null else (. * 10 | round) / 10 end;
def r2: if . == null then null else (. * 100 | round) / 100 end;
def mib:
  capture("^(?<n>[0-9.]+)\\s*(?<u>[A-Za-z]+)") as $x
  | ($x.n | tonumber) * ({"B": 1, "KiB": 1024, "kB": 1000, "MiB": 1048576, "MB": 1000000, "GiB": 1073741824, "GB": 1000000000}[$x.u] // 1) / 1048576;

$k6[0].metrics as $m
| def v($name): $m[$name].values;
  def trend($name): v($name) | if . == null then null else {p50: (.med | r1), p95: (.["p(95)"] | r1), p99: (.["p(99)"] | r1), avg: (.avg | r1), max: (.max | r1)} end;
  ($start_s + $ramp) as $t0
| ($t0 + $hold) as $t1
| [$samples[] | select(.t >= $t0 and .t <= $t1)] as $hs
| (v("http_reqs{phase:hold}").count // 0) as $reqs
| (v("errors{phase:hold}") // {}) as $err
| (($err.passes // 0)) as $err_count
| (if $reqs > 0 then $err_count / $reqs * 100 else null end) as $err_pct
| ($m.dropped_iterations.values.count // 0) as $dropped
| ($m.iterations.values.count // 0) as $iters
| (if ($dropped + $iters) > 0 then $dropped / ($dropped + $iters) * 100 else 0 end) as $drop_pct
| [$m | keys[] | capture("^http_req_duration\\{phase:hold,action:(?<a>[a-z_]+)\\}$").a] as $names
| ([$m | to_entries[] | select(.key | test("^http_reqs\\{phase:hold,action:[a-z_]+,status:[0-9]+\\}$"))
    | (.key | capture("action:(?<a>[a-z_]+),status:(?<s>[0-9]+)")) + {n: .value.values.count}]) as $statuses
| ([$names[] as $a
    | (v("http_reqs{phase:hold,action:\($a)}").count // 0) as $n
    | {key: $a, value: ({requests: $n}
        + (if $n > 0 then trend("http_req_duration{phase:hold,action:\($a)}") else {} end)
        + {errors: (v("errors{phase:hold,action:\($a)}").passes // 0),
           status_share_pct: ([$statuses[] | select(.a == $a) | {key: .s, value: (if $n > 0 then .n / $n * 100 | r2 else null end)}] | from_entries)})}]
   | from_entries) as $per_action
| ([$per_action | to_entries[] | .key as $a | .value.status_share_pct | to_entries[]
    | select((.key | startswith("2") | not) and .value != null) | {action: $a, status: .key, share_pct: .value}]) as $non2xx
| ([$non2xx[] | select(.share_pct > 5) | "\(.action): \(.status) is \(.share_pct)% of its requests (above 5%): the mix is not measuring what it claims"]) as $mix_warnings
| ([$m | keys[] | capture("^actions\\{phase:hold,intent:(?<i>[a-z_]+)\\}$").i] | map({key: ., value: $m["actions{phase:hold,intent:\(.)}"].values.count}) | from_entries) as $intents
| (($intents | add) // 0) as $intent_total
| ([("read", "write", "auth") as $k | {key: $k, value: (v("http_reqs{phase:hold,kind:\($k)}").count // 0)}] | from_entries) as $kinds
# Restarts and OOM kills during the level.
| ([range(0; ($after | length)) as $i
    | $after[$i] as $a | ($before[$i] // $a) as $b
    | select($a.restarts > $b.restarts or ($a.oom_killed and ($b.oom_killed | not)) or ($a.running | not))
    | {container: $a.name, restarts_before: $b.restarts, restarts_after: $a.restarts, oom_killed: ($a.oom_killed and ($b.oom_killed | not)), running: $a.running}]) as $bad_containers
| ([$events[] | {action: (.Action | split(" ")[0]), container: .Actor.Attributes.name, exit_code: .Actor.Attributes.exitCode}]) as $deaths
| ([$deaths[] | select(.action == "oom")] | length > 0) as $oom_event
# Node CPU: the three containers' CPU% summed (200% is the 2 cores), per sample.
| ([$hs[] | .stats |= map(select(.Name | startswith($prefix + "node1-") or startswith($prefix + "web-"))) | select((.stats | length) == $ncont) | [.stats[].CPUPerc | rtrimstr("%") | tonumber] | add]) as $cpu
# The load generator's CPU (k6 has 4 cores: 400%).
| ([$hs[] | .stats[] | select(.Name | startswith($prefix + "k6-")) | .CPUPerc | rtrimstr("%") | tonumber]) as $k6cpu
| ([$hs[].stats[] | select(.Name | startswith($prefix + "node1-") or startswith($prefix + "web-"))] | group_by(.Name) | map({key: .[0].Name, value: ([.[].MemUsage | split(" / ")[0] | mib] | max | r1)}) | from_entries) as $mem
# Per container CPU, and the box's memory: the per-sample sum, then its peak.
| ([$hs[].stats[] | select(.Name | startswith($prefix + "node1-") or startswith($prefix + "web-"))] | group_by(.Name) | map({key: .[0].Name, value: ([.[].CPUPerc | rtrimstr("%") | tonumber] | {avg: (add / length | r1), max: (max | r1)})}) | from_entries) as $cpu_by
| ([$hs[] | .stats |= map(select(.Name | startswith($prefix + "node1-") or startswith($prefix + "web-"))) | select((.stats | length) == $ncont) | [.stats[].MemUsage | split(" / ")[0] | mib] | add] | max | r1) as $box_mem
| ([$hs[] | select(.puts != null)] | group_by(((.t - $t0) / 15) | floor) | map(map(.puts) | max)) as $put_buckets
| (if ($put_buckets | length) < 2 then null else [range(1; $put_buckets | length) | select($put_buckets[.] <= $put_buckets[. - 1])] | length end) as $flat_buckets
| ([$hs[] | select(.puts != null)] | if length > 0 then (last.puts > first.puts) else null end) as $puts_rose
# Answers that were not expected for their action, by action and status (hold only).
| ([$m | to_entries[] | select(.key | test("^unexpected_status\\{phase:hold,action:[a-z_]+,status:[0-9]+\\}$"))
    | (.key | capture("action:(?<action>[a-z_]+),status:(?<status>[0-9]+)")) + {count: .value.values.count} | select(.count > 0)]) as $unexpected
| (($unexpected | map(.count) | add) // 0) as $unexpected_named
| ($err_count - $unexpected_named) as $unexpected_other
| (v("return_converted_to_borrow{phase:hold}").count // 0) as $converted
| ($m["actions{phase:hold,intent:return}"].values.count // 0) as $return_actions
| ($k6cpu | max) as $k6max
| ([
    (if $reqs == 0 then "no requests in the hold window" else empty end),
    (if $reqs > 0 and (v("http_req_duration{phase:hold}")["p(95)"] >= $p95) then "p95 \(v("http_req_duration{phase:hold}")["p(95)"] | r1) ms >= \($p95) ms" else empty end),
    (if $reqs > 0 and (v("http_req_duration{phase:hold}")["p(99)"] >= $p99) then "p99 \(v("http_req_duration{phase:hold}")["p(99)"] | r1) ms >= \($p99) ms" else empty end),
    (if $err_pct != null and $err_pct >= $err then "error rate \($err_pct | r2)% >= \($err)%" else empty end),
    (if ($bad_containers | length) > 0 or $oom_event then "node container restarted or was OOM-killed: \(($bad_containers | map(.container | ltrimstr("/"))) + ($deaths | map(.container)) | unique | join(", "))\(if $oom_event then " (Docker reported an OOM kill)" else "" end)" else empty end),
    (if $kind == "rps" and $drop_pct >= $drop then "dropped iterations \($drop_pct | r2)% >= \($drop)%" else empty end)
  ]) as $reasons
| {
    mode: $mode,
    level: $value,
    kind: $kind,
    pass: ($reasons | length == 0),
    reasons: $reasons,
    ramp_s: $ramp,
    hold_s: $hold,
    hold_requests: $reqs,
    req_per_s: (if $hold > 0 then $reqs / $hold | r1 else null end),
    latency_ms: trend("http_req_duration{phase:hold}"),
    error_pct: ($err_pct | r2),
    error_count: $err_count,
    latency_by_kind_ms: ([("read", "write", "auth") as $k | {key: $k, value: trend("http_req_duration{phase:hold,kind:\($k)}")}] | from_entries),
    # In web mode the box is nginx + web (+ Postgres): the sum of their CPU%.
    (if $mode == "web" then "box_cpu_pct_of_200" else "node_cpu_pct_of_200" end): {avg: (if ($cpu | length) > 0 then ($cpu | add / length | r1) else null end), max: ($cpu | max | r1), samples: ($cpu | length)},
    peak_memory_mib: $mem,
    cpu_by_container_pct: $cpu_by,
    box_peak_memory_mib: (if $mode == "web" then $box_mem else null end),
    nginx_access_log: $nginx_log,
    max_sync_pending: ([$hs[].pending | select(. != null)] | max),
    max_sync_lag_s: ([$hs[].lag | select(. != null)] | max | r1),
    load_generator_cpu_pct_of_400: {avg: (if ($k6cpu | length) > 0 then ($k6cpu | add / length | r1) else null end), max: ($k6max | r1), samples: ($k6cpu | length)},
    warnings: ([(if $k6max != null and $k6max > 320 then "load generator at \($k6max | r1)% of 400%: the generator may be the bottleneck" else empty end)] + $mix_warnings),
    unexpected_statuses: $unexpected,
    unexpected_other: $unexpected_other,
    return_converted_to_borrow: {count: $converted, return_actions: $return_actions, share_pct: (if $return_actions > 0 then $converted / $return_actions * 100 | r1 else null end)},
    session_pool_logged_in_at_setup: ($setup.sessions // 0),
    auth_requests_in_hold: {login: (v("http_reqs{phase:hold,action:login}").count // 0), me: (v("http_reqs{phase:hold,action:me}").count // 0), logout: (v("http_reqs{phase:hold,action:logout}").count // 0)},
    litestream: {puts_start: ([$hs[] | select(.puts != null)] | first | .puts), puts_end: ([$hs[] | select(.puts != null)] | last | .puts), puts_rose: $puts_rose, flat_15s_buckets: $flat_buckets, buckets_15s: ($put_buckets | length), txid_end: ([$hs[] | select(.txid != null)] | last | .txid)},
    drain_s: $drain,
    iterations: $iters,
    dropped_iterations: $dropped,
    dropped_pct: ($drop_pct | r2),
    request_split: {requests: $kinds, read_pct: (if $reqs > 0 then $kinds.read / $reqs * 100 | r1 else null end), write_pct: (if $reqs > 0 then $kinds.write / $reqs * 100 | r1 else null end), auth_pct: (if $reqs > 0 then $kinds.auth / $reqs * 100 | r1 else null end)},
    intended_split: {actions: $intents, read_pct: (if $intent_total > 0 then (($intents | to_entries | map(select(.key | IN("browse_books", "search_books", "open_book", "members", "list_loans")) | .value) | add // 0) / $intent_total * 100 | r1) else null end), write_pct: (if $intent_total > 0 then (($intents | to_entries | map(select(.key | IN("borrow", "return", "edit_book", "add_book", "add_member", "edit_member")) | .value) | add // 0) / $intent_total * 100 | r1) else null end)},
    per_action: $per_action,
    non_2xx_outcomes: $non2xx,
    mix_warnings: $mix_warnings,
    containers: $after,
    restarted_or_oom: $bad_containers,
    container_deaths: $deaths,
    db_rows_before: $rows_before,
    db_rows_after: $rows_after,
    samples_in_hold: ($hs | length),
    limits: {p95_ms: $p95, p99_ms: $p99, error_pct: $err, dropped_pct: $drop}
  }
