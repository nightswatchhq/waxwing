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
waxwing seal <dump-dir> [--graph-node-version V] [--public-poi 0x…]
waxwing verify <dump-dir>
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

## Next

1. The same experiment against real deployments with factories, account-like
   tables and fulltext, and a real network to compare public POIs with. Needs
   a graph-node at v0.42 or later holding synced deployments.
2. Time restore and index build per TB against a sync.
3. Dump the same final window twice a week apart and diff the rows.
4. Extend the rig: a reorg between two incremental dumps (anvil can do it),
   pruning between dumps, a grafted deployment.

Then: have `seal` fetch the public POI from the index-node itself.
