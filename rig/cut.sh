#!/usr/bin/env bash
# Rewind by file. Dump A, cut the dump back to a block that was never a
# dump head, restore the cut into B and let B index forward on its own. If
# the cut is a faithful rewind, B re-derives everything A has past it.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

setup
spawn; pings 30
wait_synced $A_INDEX
F=$(dump)
waxwing seal $DUMP --rpc $RPC >/dev/null
# Before the third child is spawned, so the cut drops a dynamic data source.
N=$((F - 35))
echo "dump at block $F"

waxwing cut $DUMP work/dumps/rig-cut --at $N --rpc $RPC
waxwing seal work/dumps/rig-cut --rpc $RPC >/dev/null
check "state root of the cut against the dump at $N" \
  "$(waxwing state work/dumps/rig-cut | tail -1)" "$(waxwing state $DUMP --at $N | tail -1)"

restore rig-cut
pings 10
wait_synced $A_INDEX; wait_synced $B_INDEX
HEAD=$(head_block)

compare $((N - 5)) "$N" $((N + 1)) "$F" "$HEAD"
dump a rig-a >/dev/null
dump b rig-b >/dev/null
waxwing diff work/dumps/rig-a work/dumps/rig-b || fail=1

# A's dump is at the chain head, which can still be reverted. Cut back to
# the finalized block it is an artefact that cannot.
waxwing seal work/dumps/rig-a --rpc $RPC >/dev/null
if waxwing verify work/dumps/rig-a --rpc $RPC --require-final >/dev/null 2>&1; then
  echo "FAIL: a dump at the chain head verified as final" >&2; fail=1
fi
waxwing cut work/dumps/rig-a work/dumps/rig-final --at final --rpc $RPC
waxwing seal work/dumps/rig-final --rpc $RPC >/dev/null
waxwing verify work/dumps/rig-final --rpc $RPC --require-final || fail=1
exit $fail
