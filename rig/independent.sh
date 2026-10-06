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

# B signs the state of its own copy with anvil's second key; A's sealed dump
# is then checked against that attestation, and against the wrong signer.
echo 0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d > work/key-b
waxwing attest work/dumps/rig-b --key-file work/key-b > work/attestation-b.json
waxwing attested work/dumps/rig-a work/attestation-b.json \
  --signer 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 || fail=1
if waxwing attested work/dumps/rig-a work/attestation-b.json \
  --signer 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266 >/dev/null; then
  echo "FAIL: an attestation counted for a signer who did not make it" >&2; fail=1
fi

# On the network, through IPFS: a fork of Arbitrum One on which a real
# indexer with stake has made B's key its operator. B signs for it and
# publishes; A's dump is checked against the CID with the fork as referee.
FORK=http://localhost:$FORK_PORT
INDEXER=0xfeff9093f6b32d0e5cddba743b06a1fedb87c004
OPERATOR=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
anvil --fork-url "${ARBITRUM_RPC:-https://arb1.arbitrum.io/rpc}" --port $FORK_PORT >/dev/null 2>&1 &
fork=$!
trap 'kill $fork 2>/dev/null' EXIT
until cast block-number --rpc-url $FORK >/dev/null 2>&1; do sleep 0.5; done
cast rpc --rpc-url $FORK anvil_impersonateAccount $INDEXER >/dev/null
cast rpc --rpc-url $FORK anvil_setBalance $INDEXER 0xde0b6b3a7640000 >/dev/null
cast send --rpc-url $FORK --unlocked --from $INDEXER 0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03 \
  "setOperator(address,address,bool)" 0xb2Bb92d0DE618878E438b55D5846cfecD9301105 $OPERATOR true >/dev/null

waxwing attest work/dumps/rig-b --key-file work/key-b --indexer $INDEXER \
  --publish $IPFS > work/attestation-op.json 2> work/publish.log
CID=$(awk '/^published/ {print $2}' work/publish.log)
echo "attestation for $INDEXER, signed by its operator, at $CID"
waxwing attested work/dumps/rig-a "$CID" --ipfs $IPFS --network-rpc $FORK || fail=1
# And on the Ethereum Attestation Service, found by deployment alone: the
# schema registered once, B's key publishing as the indexer's operator.
cast send --rpc-url $FORK --private-key $KEY 0xA310da9c5B885E7fb3fbA9D66E9Ba6Df512b78eB \
  "register(string,address,bool)" \
  "string deployment,uint32 block,bytes32 blockHash,uint32 from,bytes32 stateRoot" \
  0x0000000000000000000000000000000000000000 true >/dev/null
waxwing attest work/dumps/rig-b --key-file work/key-b --indexer $INDEXER --eas $FORK \
  > /dev/null 2> work/eas.log
echo "$(cat work/eas.log)"
waxwing attested work/dumps/rig-a --eas $FORK --network-rpc $FORK || fail=1

# B's own key has no stake: its plain attestation counts for nothing here.
if waxwing attested work/dumps/rig-a work/attestation-b.json --network-rpc $FORK >/dev/null; then
  echo "FAIL: an unstaked signer counted on the network" >&2; fail=1
fi
exit $fail
