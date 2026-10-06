#!/usr/bin/env bash
# Pruning between dumps. graph-node deletes versions closed at or before the
# new earliest block; an incremental dump records no deletion, so the
# earlier chunks still hold them.
#
# Pruned no further than the last dump's head, every pruned version was
# already closed there, and the incremental dump and one taken fresh are
# both honest: their state roots, a diff, and a restore must agree.
# Pruned past it, versions closed in between are gone with their closes,
# and seal must refuse the layer.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
# graph-node keeps at least this many blocks plus one; the toy chain is short.
export ETHEREUM_REORG_THRESHOLD=10

psql() { docker compose exec -T postgres-$1 psql -U graph-node -Atc "$2"; }
prune() { docker compose exec -T graph-node-a graphman --config /config/a.toml prune run --once --history "$1" "$DEPLOYMENT" >/dev/null; }
earliest() { jq .earliest_block_number "$1/metadata.json"; }
rows() {
  local nsp; nsp=$(psql $1 "select name from deployment_schemas where subgraph = '$DEPLOYMENT'")
  psql $1 "select (select count(*) from $nsp.child) || ' ' || (select count(*) from $nsp.stats) || ' ' || (select count(*) from $nsp.ping_event)"
}

setup
F1=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
spawn; pings 40
wait_synced $A_INDEX
prune $(( $(head_block) - F1 + 4 ))
F2=$(dump)
echo "dumps at $F1 and $F2, pruned to $(earliest $DUMP): no further than the first dump"
waxwing seal $DUMP --rpc $RPC >/dev/null || fail=1
dump a rig-fresh >/dev/null
echo "chunk rows: incremental $(jq '[.tables[].chunks[].row_count] | add' $DUMP/metadata.json), fresh $(jq '[.tables[].chunks[].row_count] | add' work/dumps/rig-fresh/metadata.json)"
check "state root, incremental against fresh" \
  "$(waxwing state $DUMP | tail -1)" "$(waxwing state work/dumps/rig-fresh | tail -1)"
waxwing diff $DUMP work/dumps/rig-fresh | tail -1
waxwing diff $DUMP work/dumps/rig-fresh >/dev/null || fail=1

docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
waxwing restore $DUMP --db $B_DB \
  --config config/b.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default | tail -1
check "rows after restore (child stats ping_event)" "$(rows a)" "$(rows b)"
pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
compare "$F2" "$(head_block)"

# Now past the last dump's head.
pings 20
wait_synced $A_INDEX
prune 15
F3=$(dump)
echo "dump at $F3, pruned to $(earliest $DUMP): past the last dump at $F2"
if waxwing seal $DUMP --rpc $RPC; then
  echo "FAIL: sealed a layer pruned past the last dump" >&2; fail=1
fi
exit $fail
