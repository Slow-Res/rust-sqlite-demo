# summary.json into summary.md.
def n: if . == null then "-" else tostring end;
def short: sub("^lt-(node1|web)-"; "") | sub("-1$"; "");
def row($cells): "| " + ($cells | map(n) | join(" | ")) + " |";
def table($head; $rows): row($head) + "\n|" + ($head | map("---") | join("|")) + "|\n" + ($rows | map(row(.)) | join("\n"));

"# Load test summary\n\n"
+ (if .aborted != null then "**ABORTED: \(.aborted)** (the levels below finished before that)\n\n" else "" end)
+ (if .node.mode == "web" then "Web only (Rust + SQLite), 2 CPUs, 4 GB, cores \(.node.cpus)." else "Node: \(.node.cpu_limit), \(.node.memory_limit), cores \(.node.cpus)." end) + " Ramp \(.ramp_s) s, hold \(.hold_s) s; every number is from the hold window only.\n"
+ "Pass rule: p95 < \(.pass_rule.p95_ms_below) ms, p99 < \(.pass_rule.p99_ms_below) ms, errors < \(.pass_rule.error_pct_below)%, no restart or OOM kill"
+ (if any(.levels[]; .kind == "rps") then ", dropped iterations < \(.pass_rule.dropped_pct_below_rps_only)%" else "" end) + ".\n\n"
+ (if .search != null then "**Search (\(.search.kind)): \(.search.status). Highest passing level: \(.search.best_pass | n) \(if .search.kind == "rps" then "actions/s" else "users" end)\(if .search.best_req_per_s != null then " (≈ \(.search.best_req_per_s) req/s)" else "" end). Lowest failing level: \(.search.first_fail | n).**\n\n" else "" end)
+ (if (.mix_warnings | length) > 0 then "**WARNING: the results may not measure what they claim.**\n" + (.mix_warnings | map("- " + .) | join("\n")) + "\n\n" else "" end)
+ "Database rows at the start: \(.db_rows_start | "books \(.books), members \(.members), loans \(.loans) (active \(.active_loans))"). At the end: \(.db_rows_end | "books \(.books), members \(.members), loans \(.loans) (active \(.active_loans))").\n\n"
+ "Active loans at the start of each level: \([.levels[] | "\(.kind) \(.level): \(.db_rows_before.active_loans | n)"] | join(", ")).\n\n## Levels\n\n"
+ table(["level", "result", "req/s", "p50 ms", "p95 ms", "p99 ms", "error %", "CPU avg / max (of 200%)", "peak memory MiB", "max sync pending / lag s", "Litestream PUTs (rose; flat 15 s buckets)", "load generator CPU avg / max (of 400%)", "drain s", "dropped"];
    [.levels[] | [
      "\(.kind) \(.level)",
      (if .pass then "PASS" else "FAIL: " + (.reasons | join("; ")) end),
      .req_per_s, .latency_ms.p50, .latency_ms.p95, .latency_ms.p99, .error_pct,
      "\(.node_cpu_pct_of_200.avg | n) / \(.node_cpu_pct_of_200.max | n)",
      (.peak_memory_mib | to_entries | map("\(.key | short) \(.value)") | join(", ")),
      (if .mode == "web" then "n/a" else "\(.max_sync_pending | n) / \(.max_sync_lag_s | n)" end),
      (if .mode == "web" or .litestream.puts_rose == null then "n/a" else "\(if .litestream.puts_rose then "yes" else "no" end); \(.litestream.flat_15s_buckets | n)/\(.litestream.buckets_15s)" end),
      "\(.load_generator_cpu_pct_of_400.avg | n) / \(.load_generator_cpu_pct_of_400.max | n)",
      (if .mode == "web" then "n/a" else .drain_s end),
      (if .kind == "rps" then "\(.dropped_iterations) (\(.dropped_pct)%)" else "-" end)
    ]])
+ "\n\n## Latency per action (hold p95 / p99 ms)\n\n"
+ (. as $s | ([$s.levels[].per_action | keys[]] | unique) as $actions
   | table(["action"] + [$s.levels[] | "\(.kind) \(.level)"];
       [$actions[] as $a | [$a] + [$s.levels[] | .per_action[$a] | if . == null then "-" else "\(.p95 | n) / \(.p99 | n)" end]]))
+ "\n\n## Expected non-2xx outcomes (share of the action's requests; unexpected answers are errors)\n\n"
+ (. as $s | ([$s.levels[].non_2xx_outcomes[] | "\(.action) \(.status)"] | unique) as $keys
   | if ($keys | length) == 0 then "None seen."
     else table(["outcome"] + [$s.levels[] | "\(.kind) \(.level)"];
       [$keys[] as $k | [$k] + [$s.levels[] | ([.non_2xx_outcomes[] | select("\(.action) \(.status)" == $k) | "\(.share_pct)%"] | first // "0%")]])
     end)
+ "\n\n## Unexpected statuses (errors; hold only)\n\n"
+ (. as $s | ([$s.levels[].unexpected_statuses[] | "\(.action) \(.status)"] | unique) as $keys
   | if ($keys | length) == 0 and all($s.levels[]; .unexpected_other == 0) then "None."
     else table(["action status"] + [$s.levels[] | "\(.kind) \(.level)"];
       [$keys[] as $k | [$k] + [$s.levels[] | ([.unexpected_statuses[] | select("\(.action) \(.status)" == $k) | .count] | first // 0)]]
       + [["other statuses"] + [$s.levels[] | .unexpected_other]])
     end)
+ "\n\n## Returns converted to a borrow (a return with no own loan left)\n\n"
+ table(["level", "converted", "return actions", "share %"]; [.levels[] | ["\(.kind) \(.level)", .return_converted_to_borrow.count, .return_converted_to_borrow.return_actions, .return_converted_to_borrow.share_pct]])
+ "\n\n## Intended versus real read/write split\n\n"
+ table(["level", "intended read / write % (actions)", "real read / write / auth % (requests)"];
    [.levels[] | ["\(.kind) \(.level)", "\(.intended_split.read_pct | n) / \(.intended_split.write_pct | n)", "\(.request_split.read_pct | n) / \(.request_split.write_pct | n) / \(.request_split.auth_pct | n)"]])
+ "\n"
