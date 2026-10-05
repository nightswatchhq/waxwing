#!/usr/bin/env bash
# A reorg between a full dump and an incremental one. The first dump is
# taken on a fork that spawns a third child; the chain then reverts and the
# canonical fork never spawns it. graphman's only guard is that the head
# number advanced, which it has, so the reverted rows stay in the directory.
# Passes when waxwing refuses to seal it.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

setup
S=$(head_block)
snapshot=$(cast rpc --rpc-url $RPC evm_snapshot | tr -d '"')

spawn; pings 20
wait_synced $A_INDEX
F1=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
echo "full dump sealed at block $F1, on the fork that will be reverted"

cast rpc --rpc-url $RPC evm_revert "$snapshot" >/dev/null
pings 40
wait_synced $A_INDEX
F2=$(dump)
echo "chain reverted to $S and rebuilt; graphman took an incremental dump at block $F2"

if waxwing seal $DUMP --rpc $RPC; then
  echo "FAIL: waxwing sealed a dump holding reverted rows" >&2; exit 1
fi
echo "waxwing refused to seal it"

# What the receiver would have been handed.
restore
pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
echo "restoring it anyway:"
compare "$S" "$F1" "$F2" "$(head_block)" >work/compare.txt 2>/dev/null
grep -v '^  ' work/compare.txt
[ $fail = 1 ] || { echo "FAIL: expected the restored copy to diverge" >&2; exit 1; }

# And from the files alone: a fresh dump of A against a dump of B.
dump a rig-a-fresh >/dev/null
dump b rig-b >/dev/null
if waxwing diff work/dumps/rig-a-fresh work/dumps/rig-b; then
  echo "FAIL: diff found nothing" >&2; exit 1
fi
