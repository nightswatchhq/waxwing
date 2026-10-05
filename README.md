# waxwing

Handing a synced subgraph deployment from one Graph indexer to another, with
enough evidence attached that the receiver can decide whether to trust it.

Waxwings pass berries down the branch from bird to bird. That is the whole
idea.

The background, the trust analysis and the three candidate designs are in
[docs/research.md](docs/research.md). This README records what has since been
checked against the code, and what exists.

## Status

One slice: `waxwing seal` and `waxwing verify`, which wrap a `graphman dump`
directory in a hashed catalogue. Tested against a synthetic dump directory
only. Nothing here has yet touched a real graph-node dump.

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

## Next

The three kill experiments from the research note, in order. All need a
graph-node with a synced deployment and a second installation to restore to.

1. Dump at a final block, seal, restore elsewhere, index 10k blocks, compare
   public POIs with the network. Choose deployments with factories, with
   account-like tables, and with fulltext.
2. Time restore and index build per TB against a sync.
3. Dump the same final window twice a week apart and diff the rows.

Then: fetch the public POI at the head block from the index-node and record
it at seal time, rather than taking it on a flag.
