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
               [--indexer 0x…] [--publish IPFS_API]
waxwing attested <dump-dir> <attestation.json|CID>... [--network-rpc URL] [--ipfs IPFS_API]
                 [--indexer 0x…]... [--threshold K]
waxwing indexes <dump-dir> --db URL [--namespace sgdN] [--apply]
waxwing restore <dump-dir> --db URL --config graphman.toml --work DIR --name NAME --node NODE
                [--graphman CMD] [--work-as PATH] [--shard SHARD] [--primary-db URL]
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
  earlier chunks still hold; see Pruning.

## The rig

`rig/run.sh` needs docker, foundry, graph-cli and about 4 GB for the VM (two
graph-nodes will not fit in colima's default 2 GB; A is OOM-killed mid-sync).
It runs two graph-node v0.45.0 installations with separate databases against
one anvil chain, and:

1. syncs a factory subgraph on A (dynamic data sources, two mutable entities,
   one immutable, a fulltext search, so both nodes run with
   `GRAPH_ALLOW_NON_DETERMINISTIC_FULLTEXT_SEARCH`), takes a full dump, then
   an incremental dump 31 blocks on;
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

`rig/arbitrum.sh` is the same handoff on a real chain, from the public RPC:
a very small subgraph over Chainlink's ETH / USD answers on Arbitrum One
(`rig/feed`), bounded by `startBlock` and `endBlock` to four finalized
hours. A syncs it; the dump is sealed against Arbitrum One and cut back to
the middle of the window, `verify --require-final` passes, and `waxwing
restore` puts the cut into B on stock v0.45.0. B indexes the second half by
itself. On 2026-10-06 A synced 60,000 blocks in 252 s; the cut restored in
0.8 s; POIs and entities agreed at the cut (28 answers) and at the end (70),
`diff` found the two nodes' dumps identical, and B's attestation at the cut
agreed with A's sealed cut.

A first attempt used Graph Horizon's own staking events, and graph-node
was OOM-killed at 15 GB, twice, on a window it should have held in 200 MB.
Indexer agents batch: one transaction there carries 600 logs, 50 of them
the subgraph's triggers, and graph-node v0.45 runs out of memory on a run
of such transactions whether or not it batches writes. Not waxwing's to
fix, but worth knowing before syncing anything agents touch.

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

The state covers history from a block on: by default the dump's earliest
block, and the version a dump keeps from before it does not count. That is
what makes a pruned indexer and an unpruned one comparable. `state --from F`
leaves out what pruning to F would delete, versions closed at or before F,
and F is part of the hash. `seal` records the state root at the head, from
the dump's earliest block, and `verify` recomputes it from the rows.
`--no-state` skips it, since hashing reads every row.

## Grafts

A grafted deployment starts as a copy of another's rows up to a block, and
its dump holds those rows. `graphman restore` nonetheless looks the graft
base up on the receiver, to choose a schema version, and fails with
"graft_base not found" where it is absent. `waxwing restore` leaves the
graft out of the skeleton graphman creates, and writes it back into the
deployment's metadata once the rows are in. The receiver gets the latest
schema version rather than the base's, which matters only for a base
created before graph-node's current layout.

`rig/graft.sh` grafts a second deployment onto the rig subgraph on A and
hands it to B, which never had the base: graphman refuses it, waxwing
restores it, and B indexes on. POIs and entities agree at the graft block,
the dump head and later, and `diff` finds the two nodes' dumps identical.

## Pruning

graph-node prunes by deleting versions closed at or before the new earliest
block. An incremental dump records no deletions, so the earlier chunks still
hold them, and `graphman restore` brings them back. Queries do not see them,
since nothing answers below the earliest block, but they are rows on disk,
and a dump that holds them differs from one taken afresh.

Pruned no further than the last dump's head, that is all: every version
pruned was already closed by then, and its close is in the chunks or a clamp
file. `state` and `diff` leave out history before the earliest block, and
`cut` and `restore` drop it. Pruned past the last dump's head, a version open
then and closed since is deleted with its close, and graphman writes clamps
only for rows that still exist: the earlier chunks hold it as live, and a
restore gives an entity two current versions. `seal` refuses such a layer.

`rig/prune.sh` prunes A to below its first dump, takes an incremental dump,
and compares it with one taken afresh: 412 rows in the chunks against 242,
yet the state roots match, `diff` finds them identical, and `waxwing restore`
gives B exactly A's rows, with POIs and entities agreeing after both index
on. Then it prunes past the second dump, and `seal` refuses the third.

## Memory

`state` and `diff` sort each table's versions on disk, in anonymous files
under `TMPDIR`, once a table passes 256 MB of them: 32 bytes a version for
`state`, 44 a side for `diff`. Clamp files are read alongside the chunks
rather than loaded, which is why chunks and clamps out of `vid` order are
refused; graph-node writes both in `vid` order. On a synthetic table of 40M
versions and 10M clamps, peak memory was 297 MB for `state` (1.7 GB
before), 558 MB for `diff` against itself (15.6 GB before) and 56 MB for
`cut`, with the same roots.

## Attestation

An indexer who synced the deployment itself runs `waxwing attest` on its own
dump: the state root as of a block, signed as an Ethereum personal message
with any secp256k1 key, so the signer is an address and `cast wallet verify`
accepts the signature. The attestation names the indexer it speaks for, the
signer itself unless `--indexer` says otherwise, and that name is part of
what is signed. `--publish` also adds it to IPFS through a Kubo API and
prints the CID.

The receiver of somebody else's dump runs `waxwing attested` with the
attestations it has collected, as files or CIDs, and gets a count of
distinct indexers who agree with the catalogue's deployment, block and
state root. With `--network-rpc`, an Arbitrum One endpoint, Graph Horizon is
the referee: an indexer counts only with tokens provisioned to the subgraph
service and not thawing (`getProviderTokensAvailable`), and only if it
signed itself or the signer is an operator it authorised there
(`isAuthorized`). An indexer and its operators count once. Without
`--network-rpc` nothing ties a key to an indexer, every key counts as
itself, and keys are free. Version 1 attestations, which name no indexer,
still verify as their signer's.

`rig/independent.sh` ends with B attesting to its own copy, and A's sealed
dump passing against it and failing against a signer who did not sign. Then
it forks Arbitrum One with anvil, has a real staked indexer authorise B's
key as its operator, and has B sign for that indexer and publish to the
rig's IPFS: A's dump, checked against the CID with the fork as referee,
counts the indexer and its stake, and B's unstaked attestation counts for
nothing. `cargo test -- --ignored` checks the staking calls against
Arbitrum One itself.

What this does not do: find attestations. A CID has to be passed along by
whoever published it; there is no index of attestations by deployment. The
stake is read at the latest block, not the attested one. The attester and
the dump must be at the same block, which is what `cut --at final` and
`attest --at` are for.

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

`graphman restore` builds graph-node's default index set before importing a
row, and ignores the `indexes` the dump records. A deployment whose operator
dropped or added indexes comes back with the defaults, and every row of the
import pays for maintaining them (graph-node#6722 is the same complaint
about copy).

`waxwing indexes <dump-dir> --db postgresql://…` fixes the first half with a
stock graph-node. It compares the restored deployment's indexes with the
dump's list and prints the statements; with `--apply` it builds what is
missing, drops the surplus, both `concurrently`, and sets
`postponed_indexes_created` so graph-node does not add postponed ones back.
It never drops an index backing a constraint, and gives BRIN indexes the
`minmax_multi_ops` operator classes where the server has them, as graph-node
does; the dump does not record them. `rig/indexes.sh` drops an attribute
index on A, adds one by hand, restores into B with stock v0.45.0, and runs
it: the two index sets then match, and POIs and entities agree after B
indexes on.

## Restore

The import time is the other half, and graphman cannot be told to build
indexes later. graph-node branch `pete/restore-dump-indexes` (on
cargopete/graph-node, against `6838f4e3c`) shows what it is worth: it
creates the tables bare and builds the dump's indexes after the import, and
is kept as a measurement, not proposed upstream.

`waxwing restore` gets the same with a stock graph-node:

1. graphman restores a skeleton: the dump's metadata with no rows and no
   head, so graphman still writes all of graph-node's own metadata. It runs
   under a copy of the operator's config with one rule put first, assigning
   this subgraph name to a node that does not exist, `waxwing_parked`.
   waxwing refuses to go on unless the deployment is parked there.
2. waxwing drops the default indexes on the entity tables, keeping those
   behind constraints, and loads every chunk with `COPY`, clamp files folded
   in.
3. It builds the dump's indexes, runs `analyze`, sets the head where
   graphman's finalize would have, and marks postponed indexes created.
4. `graphman reassign` hands the deployment to `--node`.

`--graphman` is how to run graphman (`docker exec -i graph-node graphman`),
and `--work-as` where graphman sees the work directory, if elsewhere. The
subgraph name must exist (`graphman create`).

A dump leaves out `@fulltext` columns, and graph-node computes them on
insert. waxwing reads the directives from `schema.graphql` and loads such a
table through a temporary staging table, computing each tsvector as
graph-node does: `to_tsvector` per included field, concatenated. graph-node
takes the fields in hash-set order, which varies by process, so its own
tsvectors for one row can differ in word positions between two nodes;
waxwing uses the directive's order.

An interrupted restore is resumed by running the same command again. A
deployment still parked, with no head and waxwing's config in `--work`, is
taken as one: graphman is skipped, and each table loads from past its
highest `vid`, since every `COPY` batch commits whole. Indexes are dropped
and rebuilt, and a deployment that already has its head is only reassigned.
Not handled: graph-node versions that keep the head anywhere but
`subgraphs.head`.

`rig/fast-restore.sh` takes a full and an incremental dump of A, with an
attribute index dropped in between, restores with waxwing into B on stock
v0.45.0, and lets B index on by itself. The index sets match, the
restored fulltext column matches A's and answers the same search, POIs and
entities agree at five blocks, and a final `diff` of the two nodes' dumps
is identical. B picks the deployment up from the reassignment, with no
restart.

`rig/resume.sh` pads A with 3M versions in each of two tables, kills
`waxwing restore` while it loads the second, and runs it again: on
2026-10-06 the kill came at 635,904 of 3,000,060 pings, the second run
loaded the rest, and row counts, checksums, fulltext lexemes, the index
count and the head then matched A, as did the POI and entities once both
nodes had indexed on.

`rig/restore-time.sh` pads A with 5M versions in each of two tables
(3.7 GB), dumps, and times restore into a fresh B: 369 s with graphman on
v0.45.0, 156 s with graphman on the patched build, and 140 s with
`waxwing restore` on v0.45.0. Row counts, a checksum over the pings, the
head and the index count of A and the waxwing-restored B agree. Two tables
of one toy schema: a real deployment's wider rows and more indexes may
move the ratio either way.

## Next

[docs/graphman-issues.md](docs/graphman-issues.md) is a scan of what operators
complain about in graphman. What remains of it:

1. Finding attestations: an index of them by deployment, such as a
   subgraph over an event a publisher emits.

And the experiment still owed, small and local: nested data sources (for
`parent`).
