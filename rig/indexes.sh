#!/usr/bin/env bash
# Does a restore keep the source's indexes? Drop an attribute index on A and
# add one by hand, dump, restore into B, and compare the two index sets.
# Stock graphman rebuilds the defaults, and `waxwing indexes` puts the
# dump's set back. GRAPH_NODE_IMAGE picks another graph-node build.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

psql() { docker compose exec -T postgres-$1 psql -U graph-node -Atc "$2"; }
nsp() { psql $1 "select name from deployment_schemas where subgraph = '$DEPLOYMENT'"; }
# Index definitions with the namespace taken out, which differs by node.
indexes() {
  local nsp; nsp=$(nsp $1)
  psql $1 "select indexdef from pg_indexes where schemaname = '$nsp' order by indexname" | sed "s/$nsp\./NSP./g"
}

setup
NSP=$(nsp a)
psql a "drop index $NSP.attr_2_1_ping_event_n" >/dev/null
psql a "create index manual_child_pings_created on $NSP.child using btree (pings, created_at)" >/dev/null
F=$(dump)
echo "dump at block $F"

restore
echo "after graphman restore: $(diff <(indexes a) <(indexes b) | grep -c '^[<>]') index(es) differ"
waxwing indexes $DUMP --db postgresql://graph-node:let-me-in@localhost:25432/graph-node --apply
if diff <(indexes a) <(indexes b); then echo "ok        index set"; else
  echo "MISMATCH  index set (< A, > B)"; fail=1
fi

spawn; pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
compare "$F" "$(head_block)"
exit $fail
