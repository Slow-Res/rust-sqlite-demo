# One block per level for the terminal.
def short: sub("^" + $prefix + "(node1|web)-"; "") | sub("-1$"; "");
def gib: . / 1024 | if . < 0.1 then (. * 100 | round) / 100 else (. * 10 | round) / 10 end;
def split_cpu: [.cpu_by_container_pct | to_entries | sort_by(-.value.avg)[] | "\(.key | short) \(.value.avg | round)"] | join(", ");
def split_mem: [.peak_memory_mib | to_entries | sort_by(-.value)[] | "\(.key | short) \(.value | gib)"] | join(", ");
def unit: if .kind == "rps" then "actions/s" else "users" end;
"\(.kind) \(.level): \(if .pass then "PASS" else "FAIL" end) (\(.level) \(unit) = \(.req_per_s) req/s)\(if .pass then "" else " (" + (.reasons | join("; ")) + ")" end)\n"
+ "  p50 \(.latency_ms.p50) / p95 \(.latency_ms.p95) / p99 \(.latency_ms.p99) ms  errors \(.error_pct)%\n  \(if .mode == "web" then "cpu box avg \(.box_cpu_pct_of_200.avg | round)% max \(.box_cpu_pct_of_200.max | round)% of 200% (\(split_cpu))" else "node CPU avg \(.node_cpu_pct_of_200.avg)% max \(.node_cpu_pct_of_200.max)% of 200%" end)\n"
+ (if .mode == "web" then "  mem box peak \(.box_peak_memory_mib | gib) GiB (\(split_mem))  nginx access log +\(.nginx_access_log.lines) lines\n" else "" end)
+ "  load generator (k6) CPU avg \(.load_generator_cpu_pct_of_400.avg)% max \(.load_generator_cpu_pct_of_400.max)% of 400%\n"
+ (if .mode == "web" then "  sync worker, Litestream, drain: n/a (web only)  dropped \(.dropped_iterations)\n"
   else "  max sync pending \(.max_sync_pending) lag \(.max_sync_lag_s) s  Litestream PUTs rose: \(.litestream.puts_rose) (flat 15 s buckets \(.litestream.flat_15s_buckets)/\(.litestream.buckets_15s))  drain \(.drain_s) s  dropped \(.dropped_iterations)\n" end)
+ "  active loans at the start: \(.db_rows_before.active_loans)  session pool logged in at setup: \(.session_pool_logged_in_at_setup)  login/me/logout in hold: \(.auth_requests_in_hold.login)/\(.auth_requests_in_hold.me)/\(.auth_requests_in_hold.logout)\n"
+ "  returns converted to a borrow (own stack empty): \(.return_converted_to_borrow.count) of \(.return_converted_to_borrow.return_actions) (\(.return_converted_to_borrow.share_pct // "-")%)\n"
+ "  per action (hold p95/p99 ms, requests):\n"
+ ([.per_action | to_entries[] | "    \(.key | . + (" " * (16 - length)))  \(.value.p95 // "-") / \(.value.p99 // "-")  n=\(.value.requests)\(if .value.errors > 0 then "  errors=\(.value.errors)" else "" end)"] | join("\n"))
+ "\n  expected non-2xx outcomes: " + (if (.non_2xx_outcomes | length) == 0 then "none" else ([.non_2xx_outcomes[] | "\(.action) \(.status) \(.share_pct)%"] | join(", ")) end)
+ "\n  UNEXPECTED statuses (errors): " + (if (.unexpected_statuses | length) == 0 and .unexpected_other == 0 then "none" else ([.unexpected_statuses[] | "\(.action) \(.status) x\(.count)"] + (if .unexpected_other > 0 then ["other x\(.unexpected_other)"] else [] end) | join(", ")) end)
+ "\n  request split read/write/auth: \(.request_split.read_pct)/\(.request_split.write_pct)/\(.request_split.auth_pct)%  intended read/write: \(.intended_split.read_pct)/\(.intended_split.write_pct)%"
+ (if (.warnings | length) > 0 then "\n  WARNING: " + (.warnings | join("\n  WARNING: ")) else "" end)
