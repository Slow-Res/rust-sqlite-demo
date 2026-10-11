# One line per level for the terminal.
"\(.kind) \(.level): \(if .pass then "PASS" else "FAIL" end)\(if .pass then "" else " (" + (.reasons | join("; ")) + ")" end)\n"
+ "  \(.req_per_s) req/s  p50 \(.latency_ms.p50) / p95 \(.latency_ms.p95) / p99 \(.latency_ms.p99) ms  errors \(.error_pct)%  node CPU avg \(.node_cpu_pct_of_200.avg)% max \(.node_cpu_pct_of_200.max)% of 200%\n"
+ "  max sync pending \(.max_sync_pending) lag \(.max_sync_lag_s) s  Litestream PUTs rising: \(.litestream.puts_kept_rising)  drain \(.drain_s) s  dropped \(.dropped_iterations)\n"
+ "  per action (hold p95/p99 ms, requests):\n"
+ ([.per_action | to_entries[] | "    \(.key | . + (" " * (16 - length)))  \(.value.p95 // "-") / \(.value.p99 // "-")  n=\(.value.requests)\(if .value.errors > 0 then "  errors=\(.value.errors)" else "" end)"] | join("\n"))
+ "\n  non-2xx outcomes: " + (if (.non_2xx_outcomes | length) == 0 then "none" else ([.non_2xx_outcomes[] | "\(.action) \(.status) \(.share_pct)%"] | join(", ")) end)
+ "\n  request split read/write/auth: \(.request_split.read_pct)/\(.request_split.write_pct)/\(.request_split.auth_pct)%  intended read/write: \(.intended_split.read_pct)/\(.intended_split.write_pct)%"
+ (if (.mix_warnings | length) > 0 then "\n  WARNING: " + (.mix_warnings | join("\n  WARNING: ")) else "" end)
