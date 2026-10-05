#!/usr/bin/env bash
# Two nodes that never exchange data. B follows the chain head from early on
# and lives through a reorg; A is handed the subgraph afterwards and syncs
# the whole history in one go. Their dumps should hold the same versions.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

stack_up
spawn; pings 30
deploy $B_ADMIN $B_INDEX
wait_synced $B_INDEX

snapshot=$(cast rpc --rpc-url $RPC evm_snapshot | tr -d '"')
spawn; pings 10
wait_synced $B_INDEX
cast rpc --rpc-url $RPC evm_revert "$snapshot" >/dev/null
pings 20
wait_synced $B_INDEX
spawn; pings 30
wait_synced $B_INDEX
echo "B followed the head to block $(head_block), through one reorg"

deploy $A_ADMIN $A_INDEX
wait_synced $A_INDEX
echo "A synced the same $(head_block) blocks from scratch"

HEAD=$(head_block)
check "public POI at $HEAD" "$(poi $A_INDEX "$HEAD")" "$(poi $B_INDEX "$HEAD")"
dump a rig-a >/dev/null
dump b rig-b >/dev/null
waxwing diff work/dumps/rig-a work/dumps/rig-b || fail=1

# The files differ byte for byte (vid is a sequence, and B's reorg burned
# some), so only the state root can be attested across indexers.
root() { waxwing "$1" "$2" | tail -1 | awk '{print $NF}'; }
check "state root at $HEAD" "$(root state work/dumps/rig-a)" "$(root state work/dumps/rig-b)"
[ "$(root seal work/dumps/rig-a)" != "$(root seal work/dumps/rig-b)" ] || echo "note: catalogue roots match too"
exit $fail
