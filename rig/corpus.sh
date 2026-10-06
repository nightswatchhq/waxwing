#!/usr/bin/env bash
# One real deployment through the whole handoff: republish it over a recent
# finalized window (corpus/prepare.py), sync it on A, dump, seal against the
# chain, cut to the middle, verify as final, restore into B with waxwing,
# let B index the rest, then compare public POIs and diff the two dumps.
# Usage: corpus.sh HASH NETWORK [BLOCKS]
set -euo pipefail
cd "$(dirname "$0")"
export CONFIG_SUFFIX=-corpus
# The window is final; a short reorg threshold spares the public RPC the
# node's initial download of the latest blocks.
export ETHEREUM_REORG_THRESHOLD=50
. ./lib.sh
HASH=$1 NETWORK=$2
declare -A CHAIN_RPC=([arbitrum-one]=https://arb1.arbitrum.io/rpc [optimism]=https://mainnet.optimism.io [gnosis]=https://rpc.gnosischain.com)
declare -A WINDOW=([arbitrum-one]=20000 [optimism]=3000 [gnosis]=1000)
CHAIN=${CHAIN_RPC[$NETWORK]}
BLOCKS=${3:-${WINDOW[$NETWORK]}}
NAME=corpus/$(echo "$HASH" | cut -c1-12 | tr 'A-Z' 'a-z')

status() {
  gql "$1/graphql" "{ indexingStatuses(subgraphs: [\"$DEPLOYMENT\"]) { health fatalError { message } chains { latestBlock { number } } } }"
}
latest() { status $1 | jq -r '.data.indexingStatuses[0].chains[0].latestBlock.number // 0'; }
wait_past() {
  local s at missing=0 last=-1 still=0
  for _ in $(seq 1800); do
    s=$(status $1)
    if [ "$(jq '.data.indexingStatuses | length' <<<"$s")" = 0 ]; then
      missing=$((missing + 1))
      [ $missing -gt 30 ] && { echo "FAILED: $1 is not indexing $DEPLOYMENT"; exit 2; }
    fi
    if [ "$(jq -r '.data.indexingStatuses[0].health' <<<"$s")" = failed ]; then
      echo "FAILED on $1: $(jq -r '.data.indexingStatuses[0].fatalError.message' <<<"$s")"; exit 2
    fi
    at=$(jq -r '.data.indexingStatuses[0].chains[0].latestBlock.number // 0' <<<"$s")
    [ "$at" -ge "$2" ] && return
    if [ "$at" = "$last" ]; then
      still=$((still + 1))
      [ $still -gt 300 ] && { echo "FAILED: $1 stalled at block $at for ten minutes"; exit 2; }
    else
      still=0 last=$at
    fi
    sleep 2
  done
  echo "$1 did not reach $2: $s" >&2; exit 1
}
admin() { curl -sf "$1" -H 'content-type: application/json' -d "$(jq -n --arg m "$2" --argjson p "$3" '{jsonrpc: "2.0", id: 1, method: $m, params: $p}')"; }

END=$(( $(cast block finalized --rpc-url $CHAIN --field number) - 100 ))
START=$(( END - BLOCKS ))
MID=$(( (START + END) / 2 ))

# One chain per run: idle block ingestors only wear out public RPCs' limits.
for n in a b; do
  sed -e "s|\[chains.test\]|[chains.$NETWORK]|" \
      -e "s|provider = \[ { label = \"anvil\", url = \"http://anvil:8545\", features = \[\"archive\"\] } \]|provider = [ { label = \"$NETWORK\", url = \"$CHAIN\", features = [\"archive\"] } ]|" \
      config/$n.toml > config/$n-corpus.toml
done

docker compose down -v >/dev/null 2>&1
rm -rf work/dumps 2>/dev/null || docker run --rm -v "$PWD/work:/w" alpine rm -rf /w/dumps
mkdir -p work/dumps
docker compose up -d >/dev/null 2>&1
wait_http $A_INDEX; wait_http $B_INDEX

DEPLOYMENT=$(python3 corpus/prepare.py "$HASH" "$START" "$END" "$IPFS")
admin $A_ADMIN subgraph_create "$(jq -n --arg n $NAME '{name: $n}')" >/dev/null
deployed=$(admin $A_ADMIN subgraph_deploy "$(jq -n --arg n $NAME --arg h $DEPLOYMENT '{name: $n, ipfs_hash: $h}')")
if jq -e .error <<<"$deployed" >/dev/null; then
  echo "DEPLOY FAILED: $(jq -r .error.message <<<"$deployed")"; exit 2
fi
echo "$HASH on $NETWORK as $DEPLOYMENT, blocks $START to $END"
t=$SECONDS
wait_past $A_INDEX "$END"
echo "A synced in $((SECONDS - t))s"

dump >/dev/null
waxwing seal $DUMP --rpc $CHAIN >/dev/null
waxwing cut $DUMP work/dumps/cut --at "$MID" --rpc $CHAIN >/dev/null
waxwing seal work/dumps/cut --rpc $CHAIN >/dev/null
waxwing verify work/dumps/cut --rpc $CHAIN --require-final >/dev/null
echo "cut to $MID: $(jq '[.tables[].chunks[].row_count] | add' work/dumps/cut/metadata.json) rows in $(jq '.tables | length' work/dumps/cut/metadata.json) tables"

docker compose exec -T graph-node-b graphman --config /config/b-corpus.toml create $NAME >/dev/null
waxwing restore work/dumps/cut --db $B_DB --config config/b-corpus.toml \
  --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default | tail -1
wait_past $B_INDEX "$END"

for block in "$MID" "$END"; do
  check "public POI at $block" "$(poi $A_INDEX "$block")" "$(poi $B_INDEX "$block")"
done
dump a a >/dev/null
dump b b >/dev/null
waxwing diff work/dumps/a work/dumps/b --at "$END" | tail -1
waxwing diff work/dumps/a work/dumps/b --at "$END" >/dev/null || fail=1
exit $fail
