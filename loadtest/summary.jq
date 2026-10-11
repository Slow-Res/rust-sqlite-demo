# All levels of one run (slurped) into summary.json.
. as $levels
| {
    generated_at: (now | todate),
    node: {cpus: $node_cpus, cpu_limit: "2 CPUs (cores shared by web, Litestream, sync-worker)", memory_limit: "4 GB: web 3g, litestream 512m, sync-worker 512m, restore 512m"},
    pass_rule: {p95_ms_below: $p95, p99_ms_below: $p99, error_pct_below: $err, dropped_pct_below_rps_only: $drop, no_restart_or_oom: true, window: "hold only (ramp excluded)"},
    ramp_s: $ramp,
    hold_s: $hold,
    search: $result,
    db_rows_start: ($levels | first | .db_rows_before),
    db_rows_end: ($levels | last | .db_rows_after),
    mix_warnings: ([$levels[] | .level as $l | .mix_warnings[] | "level \($l): \(.)"]),
    levels: $levels
  }
