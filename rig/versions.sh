#!/usr/bin/env bash
# fast-restore.sh across graph-node releases: each release with itself, and
# an old publisher handing to a new receiver and back. A line each in
# work/versions.txt.
cd "$(dirname "$0")"
: > work/versions.txt
while read -r a b; do
  log=work/versions-$a-$b.log
  GRAPH_NODE_IMAGE_A=graphprotocol/graph-node:$a GRAPH_NODE_IMAGE_B=graphprotocol/graph-node:$b \
    ./fast-restore.sh > "$log" 2>&1 < /dev/null
  code=$?
  first=$(grep -E "MISMATCH|FAIL|Error|error:" "$log" | head -1 | cut -c1-200)
  echo "A $a -> B $b: exit=$code ${first:-all checks ok}" >> work/versions.txt
done <<'LIST'
v0.42.1 v0.42.1
v0.43.0 v0.43.0
v0.44.0 v0.44.0
v0.42.1 v0.45.0
v0.45.0 v0.42.1
LIST
