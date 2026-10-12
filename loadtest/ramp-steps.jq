# Ramp mode: the steps $from..$to as an array of step objects. Inputs (--arg,
# --argjson, --slurpfile): k6 (a document with .metrics: the end-of-test summary,
# or the live values read from k6's REST API), samples and events (JSON lines of
# scripts/loadtest.sh), before (container state before the run), prefix, t0 (epoch
# seconds when k6's scenario started), step_s, start, step_users, from, to, ncont
# (box containers), the p95/p99/err limits, clock (the box cores' clock lock, JSON of
# scripts/loadtest.sh clock_state, or null) and source (live|final).
# Step k covers [t0 + k*step_s, t0 + (k+1)*step_s) and the requests tagged step:k.
# throttle is the growth of the box cores' (summed) and the package's throttle counters
# over the step; clock_ok is false when the clock is locked and the step ran below 95% of
# it, or when anything throttled (null: unlocked and nothing throttled, so not known).
# The REST API has no p99; there the step's metric has a p99 threshold and its
# `tainted` flag says whether the p99 is over the limit.
def r1: if . == null then null else (. * 10 | round) / 10 end;
def r2: if . == null then null else (. * 100 | round) / 100 end;
def mib:
  capture("^(?<n>[0-9.]+)\\s*(?<u>[A-Za-z]+)") as $x
  | ($x.n | tonumber) * ({"B": 1, "KiB": 1024, "kB": 1000, "MiB": 1048576, "MB": 1000000, "GiB": 1073741824, "GB": 1000000000}[$x.u] // 1) / 1048576;
def short: ltrimstr("/") | sub("^" + $prefix + "web-"; "") | sub("-1$"; "");
def is_box: .Name | startswith($prefix + "web-");
def avg: if length == 0 then null else add / length end;
def rmap: map({key: (.name | short), value: .restarts}) | from_entries;

$k6[0].metrics as $m
| ($before | rmap) as $r0
| [range($from; $to + 1) as $k
  | ($t0 + $k * $step_s) as $a
  | ($a + $step_s) as $b
  | [$samples[] | select(.t >= $a and .t < $b)] as $hs
  | $m["http_req_duration{step:\($k)}"] as $dm
  | ($dm.values // {}) as $d
  | ($m["http_req_failed{step:\($k)}"].values // {}) as $f
  | ($m["http_reqs{step:\($k)}"].values.count // 0) as $n
  | (if $f.rate == null then null else $f.rate * 100 end) as $err_pct
  | ($d["p(99)"]) as $p99
  | (if $p99 != null then $p99 >= $p99_limit else ($dm.tainted == true) end) as $p99_over
  | ([
      (if $n == 0 then "no requests in the step" else empty end),
      (if $n > 0 and $d["p(95)"] != null and $d["p(95)"] >= $p95_limit then "p95 \($d["p(95)"] | r1) ms >= \($p95_limit) ms" else empty end),
      (if $n > 0 and $p99_over then "p99 \(if $p99 != null then "\($p99 | r1) ms" else "over the limit" end) >= \($p99_limit) ms" else empty end),
      (if $err_pct != null and $err_pct >= $err_limit then "error rate \($err_pct | r2)% >= \($err_limit)%" else empty end)
    ]) as $reasons
  # CPU: each box container's CPU% (200% is the 2 cores), the box as their per-sample sum.
  | ([$hs[] | .stats |= map(select(is_box)) | select((.stats | length) == $ncont) | [.stats[].CPUPerc | rtrimstr("%") | tonumber] | add]) as $box_cpu
  | ([$hs[].stats[] | select(is_box)] | group_by(.Name) | map({key: (.[0].Name | short), value: ([.[].CPUPerc | rtrimstr("%") | tonumber] | avg | r1)}) | from_entries) as $cpu_by
  | ([$hs[].stats[] | select(is_box)] | group_by(.Name) | map({key: (.[0].Name | short), value: ([.[].MemUsage | split(" / ")[0] | mib] | max | r1)}) | from_entries) as $mem
  | ([$hs[] | .stats |= map(select(is_box)) | select((.stats | length) == $ncont) | [.stats[].MemUsage | split(" / ")[0] | mib] | add] | max | r1) as $box_mem
  | ([$hs[].stats[] | select(.Name | startswith($prefix + "k6-")) | .CPUPerc | rtrimstr("%") | tonumber]) as $k6cpu
  | ([$hs[].freq | select(. != null) | .[] | select(. != null) | . / 1000]) as $freq
  # Throttle counters are cumulative: the last sample of the step minus the last one
  # before it (or the step's first, when none is earlier).
  | ([$hs[] | select(.throttle != null)]) as $tw
  | ((([$samples[] | select(.t < $a and .throttle != null)] | last) // ($tw | first)).throttle) as $t_from
  | ($tw | last | .throttle) as $t_to
  | (if $t_from == null or $t_to == null then null
     else {core: (([$t_to.core, $t_from.core] | transpose | map(.[0] - .[1]) | add)), package: ($t_to.package - $t_from.package)} end) as $throttle
  | ([$hs[].pkg_temp_c | select(. != null)]) as $temp
  | ($freq | avg) as $freq_avg
  | ($throttle != null and ($throttle.core > 0 or $throttle.package > 0)) as $throttled
  | ($clock.locked == true and $freq_avg != null and $freq_avg < 0.95 * $clock.khz / 1000) as $drift
  | (if $throttled or $drift then false elif $clock.locked == true and $freq_avg != null then true else null end) as $clock_ok
  # Restarts: the restart count at the last sample of the step minus the one
  # before it. OOM kills: Docker's own events in the step.
  | ([$samples[] | select(.t < $a and .state != null)] | last | .state | rmap) as $prev_try
  | ($prev_try // $r0) as $prev
  | (([$hs[] | select(.state != null)] | last | .state | rmap) // $prev) as $cur
  | ([$events[] | select(.time >= $a and .time < $b and .Action == "oom") | .Actor.Attributes.name | short]) as $ooms
  | {
      step: $k,
      users: ($start + $k * $step_users),
      source: $source,
      window_start_s: ($a - $t0),
      requests: $n,
      req_per_s: ($n / $step_s | r1),
      p50: ($d.med | r1),
      p95: ($d["p(95)"] | r1),
      p99: ($p99 | r1),
      p99_over: $p99_over,
      p99_limit: $p99_limit,
      error_pct: ($err_pct | r2),
      pass: ($reasons | length == 0),
      reasons: $reasons,
      box_cpu_pct_of_200: {avg: ($box_cpu | avg | r1), max: ($box_cpu | max | r1)},
      cpu_by_container_pct: $cpu_by,
      peak_memory_mib: $mem,
      box_peak_memory_mib: $box_mem,
      k6_cpu_pct: {avg: ($k6cpu | avg | r1), max: ($k6cpu | max | r1)},
      freq_mhz: {avg: ($freq | avg | round), min: ($freq | min | round)},
      throttle: $throttle,
      pkg_temp_c: {avg: ($temp | avg | r1), max: ($temp | max | r1)},
      clock_ok: $clock_ok,
      clock: (if $throttled or $drift then ([if $throttled then "THROTTLED" else empty end, if $drift then "drift \($freq_avg | round)" else empty end] | join(" ")) elif $clock_ok then "ok" else null end),
      restarts: ($r0 | keys | map({key: ., value: (($cur[.] // 0) - ($prev[.] // 0))}) | from_entries),
      oom: ($r0 | keys | map(. as $c | {key: $c, value: ([$ooms[] | select(. == $c)] | length)}) | from_entries),
      relogins: ($m["relogins{step:\($k)}"].values.count // 0),
      samples: ($hs | length)
    }]
