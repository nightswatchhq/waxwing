#!/usr/bin/env bash
# A grafted deployment handed to a node that has never seen its base. The
# graft's dump holds the rows copied from the base, so the base should not
# be needed; graphman restore looks it up anyway.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh
# A graft must be this far below the base's head; the toy chain is short.
export ETHEREUM_REORG_THRESHOLD=10

setup
BASE=$DEPLOYMENT
G=$(( $(head_block) - 15 ))

# The rig subgraph again, grafted onto the base at G.
rm -rf work/graft-subgraph && mkdir -p work/graft-subgraph
cp -R subgraph/schema.graphql subgraph/src subgraph/abis subgraph/package.json work/graft-subgraph/
ln -s "$PWD/subgraph/node_modules" work/graft-subgraph/node_modules
sed -e 's/^features:/features:\n  - grafting/' subgraph/subgraph.yaml > work/graft-subgraph/subgraph.yaml
printf 'graft:\n  base: %s\n  block: %s\n' "$BASE" "$G" >> work/graft-subgraph/subgraph.yaml
(cd work/graft-subgraph && graph codegen >/dev/null 2>&1)
NAME=rig/graft SUBGRAPH=work/graft-subgraph deploy $A_ADMIN $A_INDEX
DEPLOYMENT=$(gql "$A_INDEX/graphql" '{ indexingStatuses { subgraph } }' | jq -r --arg base "$BASE" '[.data.indexingStatuses[].subgraph | select(. != $base)][0]')
NAME=rig/graft
echo "graft $DEPLOYMENT on $BASE at block $G"
spawn; pings 20
wait_synced $A_INDEX
F=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
echo "graft dumped at $F: base $(jq -r .graft_base $DUMP/metadata.json), block $(jq .graft_block.number $DUMP/metadata.json)"

docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
echo "graphman restore: $(docker compose exec -T graph-node-b graphman --config /config/b.toml restore /dumps/rig --name $NAME 2>&1 | tail -1)"
waxwing restore $DUMP --db postgresql://graph-node:let-me-in@localhost:25432/graph-node \
  --config config/b.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default | tail -1

pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
compare "$G" "$F" "$(head_block)"
dump a rig-a >/dev/null
dump b rig-b >/dev/null
waxwing diff work/dumps/rig-a work/dumps/rig-b || fail=1
exit $fail
