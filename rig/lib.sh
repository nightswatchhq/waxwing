# Shared by the rig's experiments. Source it from the rig directory.
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

spawn() { send $FACTORY 'spawn()'; }

# $1 pings spread over every child so far.
pings() {
  local children=() i=0 child
  while child=$(cast call --rpc-url $RPC $FACTORY 'children(uint256)(address)' $i 2>/dev/null); do
    children+=("$child"); i=$((i + 1))
  done
  for i in $(seq "$1"); do
    send "${children[$((i % ${#children[@]}))]}" 'ping()'
  done
}

# Waits for the node to be on the chain's current head, by hash: after a
# reorg the old fork can sit at the same height.
wait_synced() {
  local number hash status
  number=$(head_block)
  hash=$(cast block --rpc-url $RPC "$number" --field hash)
  for _ in $(seq 180); do
    status=$(gql "$1/graphql" "{ indexingStatuses(subgraphs: [\"$DEPLOYMENT\"]) { health chains { latestBlock { number hash } } } }")
    [ "$(jq -r '.data.indexingStatuses[0].health' <<<"$status")" = failed ] && { echo "$1 failed: $status" >&2; exit 1; }
    [ "0x$(jq -r '.data.indexingStatuses[0].chains[0].latestBlock.hash // ""' <<<"$status")" = "$hash" ] && return
    sleep 1
  done
  echo "$1 did not reach block $number ($hash): $status" >&2; exit 1
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

# dump [node] [directory under work/dumps]; prints the head it was taken at.
dump() {
  local node=${1:-a} name=${2:-rig}
  docker compose exec -T graph-node-$node graphman --config /config/$node.toml dump "$DEPLOYMENT" /dumps/$name >/dev/null
  jq .head_block.number work/dumps/$name/metadata.json
}

restore() {
  docker compose exec -T graph-node-b graphman --config /config/b.toml create $NAME >/dev/null
  docker compose exec -T graph-node-b graphman --config /config/b.toml restore /dumps/rig --name $NAME >/dev/null
  docker compose restart graph-node-b >/dev/null 2>&1
  wait_http $B_INDEX
}

# Fresh stack, factory deployed, two children with 60 pings, subgraph on A.
setup() {
  docker compose down -v >/dev/null 2>&1
  rm -rf work/dumps && mkdir -p work/dumps
  docker compose up -d >/dev/null 2>&1
  wait_http $A_INDEX; wait_http $B_INDEX

  forge create --root contracts --rpc-url $RPC --private-key $KEY --broadcast src/Rig.sol:Factory >/dev/null
  [ "$(cast code --rpc-url $RPC $FACTORY)" != 0x ] || { echo "factory not at $FACTORY" >&2; exit 1; }

  spawn; pings 30; spawn; pings 30
  (cd subgraph && graph create --node $A_ADMIN $NAME >/dev/null 2>&1 &&
    graph deploy $NAME --node $A_ADMIN --ipfs http://localhost:15001 --version-label v1 >/dev/null 2>&1)
  DEPLOYMENT=$(gql $A_INDEX/graphql '{ indexingStatuses { subgraph } }' | jq -r '.data.indexingStatuses[0].subgraph')
  echo "deployment $DEPLOYMENT"
  wait_synced $A_INDEX
}

fail=0
check() {
  if [ "$2" = "$3" ] && [ -n "$2" ] && [ "$2" != null ]; then echo "ok        $1"; else
    echo "MISMATCH  $1"; echo "  A: $2"; echo "  B: $3"; fail=1
  fi
}

# Public POIs and entity sets on A and B at each block given.
compare() {
  local block a
  for block in "$@"; do
    check "public POI at $block" "$(poi $A_INDEX "$block")" "$(poi $B_INDEX "$block")"
    a=$(entities $A_QUERY "$block")
    check "entities at $block ($(jq -r '"\(.childs | length) children, \(.pingEvents | length) pings"' <<<"$a"))" \
      "$a" "$(entities $B_QUERY "$block")"
  done
}
