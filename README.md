# waxwing

Handing a synced subgraph deployment from one Graph indexer to another, with
enough evidence attached that the receiver can decide whether to trust it.

Waxwings pass berries down the branch from bird to bird. That is the whole
idea.

The background, the trust analysis and the three candidate designs are in
[docs/research.md](docs/research.md). This README records what has since been
checked against the code, and what exists.

## Status

`waxwing seal` and `waxwing verify` wrap a `graphman dump` directory in a
hashed catalogue. `rig/run.sh` is the first kill experiment in miniature, and
it passes; see below.

```
waxwing seal <dump-dir> [--rpc URL] [--no-state] [--graph-node-version V] [--public-poi 0x…]
waxwing verify <dump-dir> [--rpc URL [--require-final]]
waxwing diff <dump-dir-a> <dump-dir-b> [--at BLOCK]
waxwing state <dump-dir> [--at BLOCK]
waxwing cut <src-dump-dir> <dst-dir> --at BLOCK|final [--rpc URL]
waxwing attest <own-dump-dir> --key-file FILE [--at BLOCK|final] [--rpc URL]
waxwing attested <dump-dir> <attestation.json>... [--signer 0x…]... [--threshold K]
```

`seal` reads the dump's `metadata.json`, hashes every file it references
(chunks, clamps, schema, manifest, and `metadata.json` itself) and writes
`catalogue.json` beside it. `verify` re-hashes and exits non-zero on any
missing, truncated, altered or unlisted file.

The catalogue's `root` identifies one publisher's artefact. It is not a state
commitment: two honest indexers will produce different Parquet bytes for the
same deployment. Agreement between publishers needs a canonical row encoding,
which is not built.

## What the graph-node code says

Read at graph-node `6838f4e3c` (2026-08-25), in
`store/postgres/src/relational/{dump,restore}.rs` and `docs/dump.md`. This
settles the largest unknown in the research note.

- **Incremental dumps are real, and they are a change log.** A re-run exports
  rows with `vid > max_vid` into new chunk files, and for mutable tables
  writes `clamp_NNNNNN.parquet` files of `(vid, block_range_end)` for versions
  closed since the previous dump. That is Design A's inserts-plus-closes,
  already written. `docs/dump.md` does not mention clamps; the code does.
- **`Poi$` and `data_sources$` are included.** The POI table is dumped as an
  ordinary entity directory.
- **Restore sets the head pointer, `earliest_block_number`, graft base and
  block, and resets the vid sequences.** It validates nothing about the data.
- **Fulltext columns are skipped and rebuilt on restore.**
- **Restore builds the default index set**, ignoring the indexes recorded in
  the dump. The docs warn this makes large restores slow.
- **The dump runs in a single transaction** against whatever it is pointed at.

What the dump does not have, and waxwing therefore has to add:

- No content hashes. Chunks are identified by vid range and row count.
- No finality. It dumps at the current head. The only reorg guard on an
  incremental run is "refuse if the head number has not advanced", so a reorg
  that reverts and then passes the previous head between two runs is not
  caught, and the reverted rows stay in the earlier chunk files. This is a
  reading of the guard, not a reproduced failure.
- No graph-node version and no POI recorded in the metadata.
- No account of pruning between incremental runs, which deletes rows that
  earlier chunks still hold.

## The rig

`rig/run.sh` needs docker, foundry, graph-cli and about 4 GB for the VM (two
graph-nodes will not fit in colima's default 2 GB; A is OOM-killed mid-sync).
It runs two graph-node v0.45.0 installations with separate databases against
one anvil chain, and:

1. syncs a factory subgraph on A (dynamic data sources, two mutable entities,
   one immutable), takes a full dump, then an incremental dump 31 blocks on;
2. seals the directory with the public POI at the dump head, and verifies it;
3. restores it into B, which has never been told about the subgraph, and lets
   both index a further 31 blocks;
4. compares public POIs and full entity sets between A and B at five blocks:
   before and at each dump head, and at the final head.

Result on 2026-10-05: all ten comparisons match. B answers time-travel queries
below the restore point, so the clamp files were applied, and it indexes pings
from children spawned before the restore, so `data_sources$` came across.

What this does not show: anything about scale, pruned or grafted deployments,
fulltext, file data sources, reorgs between dumps, or a real chain. It is one
toy subgraph of 125 blocks. It also needed two things the dump docs do not
mention: `graphman create <name>` on the receiver before `restore --name`,
and the receiver must be able to fetch the manifest and WASM from IPFS, since
the dump carries neither.

`rig/reorg.sh` puts a reorg between the full dump and the incremental one.
graphman accepts it, because the head number advanced, and the restored copy
is corrupt: an entity from the reverted fork, two live versions of one entity,
and diverging POIs, although the POI at the dump head still matches. The
script passes when `waxwing seal --rpc` refuses the directory.

## Layers

`seal` must run after every `graphman dump` into a directory. Each run becomes
a layer recording the head it was dumped at, since graphman keeps only the
latest. With `--rpc`, `seal` and `verify` check every layer's head is still on
the chain, and `seal` refuses a directory holding a dump it never witnessed.
A head that is canonical today can still be reorged tomorrow; see Finality.

## Diff

`waxwing diff` compares two dumps of one deployment from the files alone, as
both stood at the lower of their heads, and names the first block at which
they disagree, per table, with a few of the offending versions. It ignores
`vid` and row order, applies clamp files, and treats `Poi$` and
`data_sources$` as tables like any other, so a POI divergence shows up as a
block number. Neither party needs access to the other's node, which is what
graph-node#6694 asks for and cannot have through the status API.

On the rig: a dump of A against a dump of the copy restored into B is
identical across all five tables at block 125. After the reorg experiment it
reports the first divergence at block 64, the block after the fork point, and
shows the child that only ever existed on the reverted fork.

Limits: it holds a 32-byte key per entity version in memory for both sides of
one table, so it is not yet fit for a table of a billion rows. It refuses
dumps pruned to different blocks.

## State root

`rig/independent.sh` syncs the subgraph on two nodes that never exchange
data: B follows the head from early on and lives through a reorg, A syncs the
whole history afterwards. Their public POIs match and `diff` finds them
identical. Their dump files do not match byte for byte, and cannot: `vid` is
a sequence, and B's reorg consumed values A never used (max 93 against 82 in
one table). So the catalogue root identifies an artefact and nothing more.

`waxwing state` is the thing two indexers can agree on: a hash over every
entity version as of a block, independent of `vid`, row order, Parquet
encoding and whether a close arrived in a chunk or a clamp file. On the rig
the two nodes produce the same state root at their head and at an earlier
block. This is the state commitment the POI is not, and what multi-party
attestation would sign. `data_sources$.parent` is still untested: it was null
in every row the rig produced.

`seal` records the state root at the head in the catalogue, and `verify`
recomputes it from the rows. `--no-state` skips it for dumps too large to
hash in memory.

## Attestation

An indexer who synced the deployment itself runs `waxwing attest` on its own
dump: the state root as of a block, signed as an Ethereum personal message
with any secp256k1 key, so the signer is an address and `cast wallet verify`
accepts the signature. The receiver of somebody else's dump runs
`waxwing attested` with the attestations it has collected and the addresses
it is prepared to believe, and gets a count of distinct signers who agree
with the catalogue's deployment, block and state root.

`rig/independent.sh` ends with B attesting to its own copy and A's sealed
dump passing against it, and failing against a signer who did not sign.

What this does not do: decide who is worth believing. Without `--signer`
every key counts, and keys are free. Tying a signer to an indexer with stake
on the network is not built, nor is any way to publish or find attestations.
The attester and the dump must be at the same block, which is what
`cut --at final` and `attest --at` are for.

## Cut

`waxwing cut` writes the dump a deployment would have had at an earlier
block: versions that began later are dropped, closes that came later are
undone, clamp files are folded in, and the head pointer and entity count are
rewritten. Restoring the result is a rewind that never touches the source
deployment, and a copy at a chosen block height (graph-node#6085). It mirrors
what graph-node's own revert does to the tables, including leaving
`done_at` on data sources alone.

`rig/cut.sh` dumps A at block 94, cuts the dump back to block 59 (before the
third child is spawned, so a dynamic data source is dropped), restores the
cut into B and lets B index forward by itself. B re-derives everything: POIs
and entities match A at five blocks either side of the cut, and a final
`diff` of both nodes' dumps is identical at block 104. The cut's state root
equals `state --at 59` of the uncut dump.

With `--rpc`, cut first checks that the dump's head and every sealed layer are
on that chain, so the block it cuts at is an ancestor of what was indexed and
the chain's hash for it is the right one. Without, it will only cut at a
block the dump was sealed at.

## Finality

graphman dumps at the chain head, which is never final on a live deployment,
so refusing to seal a non-final head would refuse everything. Instead:

- `waxwing cut --at final --rpc URL` cuts a dump back to the chain's
  finalized block. The result holds nothing that can be reverted, and is the
  thing to publish and to attest a state root for.
- `waxwing verify --rpc URL` says whether the head is final on the
  receiver's own chain, and fails on a non-final one with `--require-final`.

A non-final dump is still restorable: graph-node reverts a restored
deployment like any other if its head is reorged. What it cannot safely take
is a further incremental layer, which is what the layer check guards.

`rig/cut.sh` ends by cutting A's head dump (block 104) back to anvil's
finalized block (40): the head dump fails `--require-final`, the cut passes.

## Indexes

Not solved, and not solvable from outside. `graphman restore` builds the
default index set whatever the dump records (graph-node#6722 is the same
complaint about copy). waxwing could drop the surplus afterwards, which
recovers the disk but not the hours spent building them. The fix belongs in
graph-node's restore: create only the indexes in the dump's metadata.

## Next

[docs/graphman-issues.md](docs/graphman-issues.md) is a scan of what operators
complain about in graphman. What remains of it:

1. Indexes on restore, as a graph-node patch.
2. Diff, state and cut for large tables: bounded memory, sort and merge on
   disk. Cut already streams; diff and state do not.
3. Attestations: check a signer is an indexer or its operator on the
   network, and somewhere to publish and find them.

And the experiments still owed: nested data sources (for `parent`), real
deployments on a real network, restore time per TB, pruning between dumps, a
grafted deployment.
