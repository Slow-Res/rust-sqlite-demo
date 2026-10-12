# Ramp mode with WARMUP_S: the warm-up as one object. It is never judged and is no step.
# Inputs (--arg, --argjson, --slurpfile): k6 (a document with .metrics, as for
# ramp-steps.jq), samples (JSON lines of scripts/loadtest.sh), prefix, t0 (epoch seconds
# when k6's scenario started), warmup_s, start and ncont (box containers).
# Covers [t0, t0 + warmup_s) and the requests tagged step:warmup. Logins are the ones
# that started in the warm-up (k6's warmup_logins counter).
include "ramp-util";

$k6[0].metrics as $m
| ($t0 + $warmup_s) as $b
| [$samples[] | select(.t >= $t0 and .t < $b)] as $hs
| ($m["http_reqs{step:warmup}"].values.count // 0) as $n
| ($m["http_req_failed{step:warmup}"].values // {}) as $f
| ([$hs[] | .stats |= map(select(is_box)) | select((.stats | length) == $ncont) | [.stats[].CPUPerc | rtrimstr("%") | tonumber] | add]) as $box_cpu
| {
    warmup_s: $warmup_s,
    users: $start,
    requests: $n,
    req_per_s: ($n / $warmup_s | r1),
    p95: ($m["http_req_duration{step:warmup}"].values["p(95)"] | r1),
    error_pct: (if $f.rate == null then null else $f.rate * 100 | r2 end),
    box_cpu_pct_of_200: {avg: ($box_cpu | avg | r1), max: ($box_cpu | max | r1)},
    logins: $m["warmup_logins"].values.count,
    samples: ($hs | length)
  }
