#!/usr/bin/env bash
# Kill experiment 1 in miniature: sync on A, dump, incremental dump, seal,
# restore into B, let both index on, then compare public POIs and entities.
set -euo pipefail
cd "$(dirname "$0")"
. ./lib.sh

setup
F1=$(dump)
echo "full dump at block $F1"
waxwing seal $DUMP --rpc $RPC >/dev/null

spawn; pings 30
wait_synced $A_INDEX
F2=$(dump)
echo "incremental dump at block $F2: $(find $DUMP -name 'clamp_*' | wc -l | tr -d ' ') clamp file(s)"

waxwing seal $DUMP --rpc $RPC --graph-node-version v0.45.0 --public-poi "$(poi $A_INDEX "$F2")"
waxwing verify $DUMP --rpc $RPC

restore
spawn; pings 30
wait_synced $A_INDEX; wait_synced $B_INDEX

compare $((F1 - 10)) "$F1" $((F2 - 10)) "$F2" "$(head_block)"
exit $fail
