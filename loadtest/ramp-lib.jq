# Shared by ramp-line.jq and ramp-summary.jq: one step object (ramp-steps.jq) as
# a row of the per-step table. The terminal prints the same row as summary.md.
def n: if . == null then "-" else tostring end;
def split_of: [to_entries | sort_by(-.value)[] | "\(.key) \(.value | round)"] | join(", ");
def p99_cell: if .p99 != null then .p99 else (if .p99_over then ">= " else "< " end) + "\(.p99_limit)" end;
def cells: [
  .users, .req_per_s, .p50, .p95, p99_cell, .error_pct,
  "\(.box_cpu_pct_of_200.avg | n) (\(.cpu_by_container_pct | split_of))",
  (.peak_memory_mib | split_of),
  "\(.k6_cpu_pct.avg | n)",
  "\(.freq_mhz.avg | n) / \(.freq_mhz.min | n)",
  "\([.restarts[]] | add) / \([.oom[]] | add)",
  .relogins,
  (if .pass then "PASS" else "FAIL (" + (.reasons | join("; ")) + ")" end)
];
def row: "| " + (cells | map(n) | join(" | ")) + " |";
def head: ["users", "req/s", "p50 ms", "p95 ms", "p99 ms", "err %", "box CPU % of 200 (avg; per container)", "peak MiB per container", "k6 CPU %", "CPU MHz avg / min", "restarts / OOM", "re-logins", "result"];
def header: "| " + (head | join(" | ")) + " |\n|" + (head | map("---") | join("|")) + "|";
