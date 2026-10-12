# Shared by ramp-steps.jq and ramp-warmup.jq: rounding, sizes and the box's container names.
def r1: if . == null then null else (. * 10 | round) / 10 end;
def r2: if . == null then null else (. * 100 | round) / 100 end;
def mib:
  capture("^(?<n>[0-9.]+)\\s*(?<u>[A-Za-z]+)") as $x
  | ($x.n | tonumber) * ({"B": 1, "KiB": 1024, "kB": 1000, "MiB": 1048576, "MB": 1000000, "GiB": 1073741824, "GB": 1000000000}[$x.u] // 1) / 1048576;
def short: ltrimstr("/") | sub("^" + $prefix + "web-"; "") | sub("-1$"; "");
def is_box: .Name | startswith($prefix + "web-");
def avg: if length == 0 then null else add / length end;
def rmap: map({key: (.name | short), value: .restarts}) | from_entries;
