#!/usr/bin/env bash
# A very small real subgraph on a real chain: Chainlink's ETH / USD answers
# on Arbitrum One, through the public RPC, over a finalized window
# (startBlock to endBlock in feed/subgraph.yaml). A
# syncs it; its dump is sealed against the chain and cut back to the middle
# of the window, which waxwing restores into B on stock graph-node. B
# indexes the second half by itself; then the two are compared, and B
# attests to A's cut.
set -euo pipefail
cd "$(dirname "$0")"
export CONFIG_SUFFIX=-arbitrum SUBGRAPH=feed
. ./lib.sh
ARB=https://arb1.arbitrum.io/rpc

status() {
  gql "$1/graphql" "{ indexingStatuses(subgraphs: [\"$DEPLOYMENT\"]) { health fatalError { message } chains { latestBlock { number } chainHeadBlock { number } } } }"
}
latest() { status $1 | jq -r '.data.indexingStatuses[0].chains[0].latestBlock.number // 0'; }
# Until the node is past block $2, or within 20 blocks of the chain head.
wait_past() {
  local s
  for _ in $(seq 900); do
    s=$(status $1)
    [ "$(jq -r '.data.indexingStatuses[0].health' <<<"$s")" = failed ] && { echo "$1 failed: $s" >&2; exit 1; }
    local at head
    at=$(jq -r '.data.indexingStatuses[0].chains[0].latestBlock.number // 0' <<<"$s")
    head=$(jq -r '.data.indexingStatuses[0].chains[0].chainHeadBlock.number // 0' <<<"$s")
    if [ -n "${2:-}" ]; then [ "$at" -ge "$2" ] && return; elif [ "$head" -gt 0 ] && [ $((head - at)) -le 20 ]; then return; fi
    sleep 2
  done
  echo "$1 did not catch up: $s" >&2; exit 1
}
feed() {
  gql "$1/subgraphs/id/$DEPLOYMENT" "{
    feeds(first: 1000, orderBy: id, block: {number: $2}) { id answer rounds updatedAt }
    rounds(first: 1000, orderBy: id, block: {number: $2}) { id feed { id } roundId answer updatedAt block }
  }" | jq -ecS 'if .errors then error(.errors | tostring) else .data end'
}

docker compose down -v >/dev/null 2>&1
rm -rf work/dumps && mkdir -p work/dumps
docker compose up -d >/dev/null 2>&1
wait_http $A_INDEX; wait_http $B_INDEX
deploy $A_ADMIN $A_INDEX
echo "deployment $DEPLOYMENT"
START=$(awk '/startBlock/ {print $2}' feed/subgraph.yaml)
END=$(awk '/endBlock/ {print $2}' feed/subgraph.yaml)
MID=$(( (START + END) / 2 ))
start=$SECONDS
wait_past $A_INDEX "$END"
echo "A synced to $(latest $A_INDEX) in $((SECONDS - start))s"

H=$(dump)
waxwing seal $DUMP --rpc $ARB >/dev/null
waxwing cut $DUMP work/dumps/arb-cut --at "$MID" --rpc $ARB
waxwing seal work/dumps/arb-cut --rpc $ARB
waxwing verify work/dumps/arb-cut --rpc $ARB --require-final
echo "dumped at $H, cut to $MID"

docker compose exec -T graph-node-b graphman --config /config/b-arbitrum.toml create $NAME >/dev/null
waxwing restore work/dumps/arb-cut --db $B_DB \
  --config config/b-arbitrum.toml --graphman "docker compose exec -T graph-node-b graphman" \
  --work work/dumps/waxwing --work-as /dumps/waxwing --name $NAME --node default | tail -1

# B indexes the second half of the window by itself.
wait_past $B_INDEX "$END"
for block in "$MID" "$END"; do
  check "public POI at $block" "$(poi $A_INDEX "$block")" "$(poi $B_INDEX "$block")"
  a=$(feed $A_QUERY "$block")
  check "entities at $block ($(jq -r '"\(.feeds[0].rounds // 0) rounds, answer \(.feeds[0].answer // "none")"' <<<"$a"))" \
    "$a" "$(feed $B_QUERY "$block")"
done

dump a arb-a >/dev/null
dump b arb-b >/dev/null
waxwing diff work/dumps/arb-a work/dumps/arb-b || fail=1

# B, having indexed past MID itself, attests to the state at MID; A's cut agrees.
echo 0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d > work/key-b
waxwing seal work/dumps/arb-b --rpc $ARB >/dev/null
waxwing attest work/dumps/arb-b --key-file work/key-b --at "$MID" --rpc $ARB > work/attestation-arb.json
waxwing attested work/dumps/arb-cut work/attestation-arb.json || fail=1
exit $fail
