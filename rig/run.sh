#!/usr/bin/env bash
# Kill experiment 1 in miniature: sync on A, dump, incremental dump, seal,
# restore into B, let both index on, then compare public POIs and entities.
set -euo pipefail
cd "$(dirname "$0")"

RPC=http://localhost:18545
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
FACTORY=0x5FbDB2315678afecb367f032d93F642f64180aa3
NAME=rig/test
DUMP=work/dumps/rig
A_QUERY=http://localhost:18000 A_ADMIN=http://localhost:18020 A_INDEX=http://localhost:18030
B_QUERY=http://localhost:28000 B_INDEX=http://localhost:28030

waxwing() { cargo run -q --manifest-path ../Cargo.toml -- "$@"; }
gql() { curl -sf "$1" -H 'content-type: application/json' -d "$(jq -n --arg q "$2" '{query: $q}')"; }
send() { cast send --rpc-url $RPC --private-key $KEY "$@" >/dev/null; }
head_block() { cast block-number --rpc-url $RPC; }

wait_http() {
  for _ in $(seq 120); do
    gql "$1/graphql" '{ indexingStatuses { subgraph } }' >/dev/null 2>&1 && return
    sleep 1
  done
  echo "timed out waiting for $1" >&2; exit 1
}

# One new child, then $1 pings spread over every child so far.
activity() {
  send $FACTORY 'spawn()'
  local children=() i=0
  while child=$(cast call --rpc-url $RPC $FACTORY 'children(uint256)(address)' $i 2>/dev/null); do
    children+=("$child"); i=$((i + 1))
  done
  for i in $(seq "$1"); do
    send "${children[$((i % ${#children[@]}))]}" 'ping()'
  done
}

wait_synced() {
  local target status
  target=$(head_block)
  for _ in $(seq 180); do
    status=$(gql "$1/graphql" "{ indexingStatuses(subgraphs: [\"$DEPLOYMENT\"]) { health chains { latestBlock { number } } } }")
    [ "$(jq -r '.data.indexingStatuses[0].health' <<<"$status")" = failed ] && { echo "$1 failed: $status" >&2; exit 1; }
    [ "$(jq -r '.data.indexingStatuses[0].chains[0].latestBlock.number // -1' <<<"$status")" -ge "$target" ] && return
    sleep 1
  done
  echo "$1 did not reach block $target: $status" >&2; exit 1
}

poi() {
  gql "$1/graphql" "{ publicProofsOfIndexing(requests: [{deployment: \"$DEPLOYMENT\", blockNumber: $2}]) { proofOfIndexing } }" |
    jq -r '.data.publicProofsOfIndexing[0].proofOfIndexing'
}

entities() {
  gql "$1/subgraphs/id/$DEPLOYMENT" "{
    stats(id: \"stats\", block: {number: $2}) { spawned pings }
    childs(first: 1000, orderBy: id, block: {number: $2}) { id pings createdAt }
    pingEvents(first: 1000, orderBy: id, block: {number: $2}) { id child { id } n block }
  }" | jq -ecS 'if .errors then error(.errors | tostring) else .data end'
}

docker compose down -v >/dev/null 2>&1
rm -rf work/dumps && mkdir -p work/dumps
docker compose up -d >/dev/null 2>&1
wait_http $A_INDEX; wait_http $B_INDEX

forge create --root contracts --rpc-url $RPC --private-key $KEY --broadcast src/Rig.sol:Factory >/dev/null
[ "$(cast code --rpc-url $RPC $FACTORY)" != 0x ] || { echo "factory not at $FACTORY" >&2; exit 1; }

activity 30; activity 30
(cd subgraph && graph create --node $A_ADMIN $NAME >/dev/null &&
  graph deploy $NAME --node $A_ADMIN --ipfs http://localhost:15001 --version-label v1 >/dev/null)
DEPLOYMENT=$(gql $A_INDEX/graphql '{ indexingStatuses { subgraph } }' | jq -r '.data.indexingStatuses[0].subgraph')
echo "deployment $DEPLOYMENT"

wait_synced $A_INDEX
docker compose exec -T graph-node-a graphman --config /config/a.toml dump "$DEPLOYMENT" /dumps/rig >/dev/null
F1=$(jq .head_block.number $DUMP/metadata.json)
echo "full dump at block $F1"

activity 30
wait_synced $A_INDEX
docker compose exec -T graph-node-a graphman --config /config/a.toml dump "$DEPLOYMENT" /dumps/rig >/dev/null
F2=$(jq .head_block.number $DUMP/metadata.json)
echo "incremental dump at block $F2: $(find $DUMP -name 'clamp_*' | wc -l | tr -d ' ') clamp file(s)"

waxwing seal $DUMP --graph-node-version v0.45.0 --public-poi "$(poi $A_INDEX "$F2")"
waxwing verify $DUMP

docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
docker compose exec -T graph-node-b graphman --config /config/b.toml restore /dumps/rig --name $NAME >/dev/null
docker compose restart graph-node-b >/dev/null 2>&1
wait_http $B_INDEX

activity 30
wait_synced $A_INDEX; wait_synced $B_INDEX
F3=$(head_block)

fail=0
check() {
  if [ "$2" = "$3" ] && [ -n "$2" ] && [ "$2" != null ]; then echo "ok        $1"; else
    echo "MISMATCH  $1"; echo "  A: $2"; echo "  B: $3"; fail=1
  fi
}
for block in $((F1 - 10)) "$F1" $((F2 - 10)) "$F2" "$F3"; do
  check "public POI at $block" "$(poi $A_INDEX "$block")" "$(poi $B_INDEX "$block")"
  a=$(entities $A_QUERY "$block")
  check "entities at $block ($(jq -r '"\(.childs | length) children, \(.pingEvents | length) pings"' <<<"$a"))" "$a" "$(entities $B_QUERY "$block")"
done
exit $fail
