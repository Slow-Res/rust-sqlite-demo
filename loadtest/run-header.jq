# Shared by summary-md.jq and ramp-summary.jq: the two lines about the run's start
# (scripts/loadtest.sh preflight): whether the box cores' clock was locked, and what the
# cool-down gate did. Both are the JSON the script recorded, or null.
def run_header($clock; $cool):
  "CPU clock: \(if $clock == null then "not recorded" else $clock.text end). \(if $cool == null then "Cool-down gate: not recorded" else ($cool.text | ascii_upcase[0:1] + .[1:]) end).\n";
