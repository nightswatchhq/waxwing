# What operators complain about in graphman

A scan of `graphprotocol/graph-node` issues mentioning graphman, taken
2026-10-05: 42 open, plus those closed since June 2025. Issue titles and the
bodies of seven were read; comment threads were not. One Discord thread from
3-5 October 2026 is included because it prompted the scan.

The point is to find where waxwing can be better than what exists, and to be
plain about where it cannot help.

## 1. Moving a deployment is expensive and lossy

| Issue | State | Complaint |
| --- | --- | --- |
| #6722 | open | Since v0.44.0 `graphman copy` rebuilds the default index set, ignoring the source's. +1.35 TB on a 1.6 TB deployment. |
| #5140 | open | The original request that copy should follow the source's indexes. Open since January 2024. |
| #6085 | open | Copy a deployment at a chosen block height, as a safer alternative to rewind. |
| #5269 | open | Copy refuses a deployment in a failed state, then `graphman info` panics. |
| #5734 | open | Copying the network subgraph between shards returns empty fulltext results. |
| #6366 | closed | Copy carried a pruned `earliest_block_number` to an invalid destination. |
| #6609 | closed | Dump/restore could not handle larger subgraphs (fixed by #6646 in v0.45.0). |

`graphman restore` has the #6722 problem by design: the dump records the
source's index definitions and restore ignores them, as `docs/dump.md` admits.

**Where waxwing can win.** A restore that builds exactly the indexes the
catalogue records, and a restore to a chosen block at or below the head. The
dump holds every version with its block range, so cutting it at block N is a
filter over Parquet: keep rows whose range starts at or before N, reopen
ranges that close after N. That is #6085 without touching the source.

## 2. Nobody can compare two copies

| Issue | State | Complaint |
| --- | --- | --- |
| #6694 | open | The POI API resolves a deployment hash to the active copy only, so two copies can never be compared as they stand. "Verifying a PoI is inherently a comparison between two datasets, and the API only exposes one of them at a time." |
| #4938 | open | A `graphman database check` was asked for in October 2023. |

**Where waxwing can win.** `waxwing diff` between two sealed dumps, or a dump
and a live deployment: first block at which entity versions or the stored POI
digest part ways, per table. It needs no access to the other party's node,
which is also what cross-indexer attestation needs. This is the strongest fit
in the whole list.

## 3. Rewind is feared

| Issue | State | Complaint |
| --- | --- | --- |
| #6334 | closed | Rewind on a pruned subgraph corrupted entity state. |
| #6088, #6365 | closed | Rewinding one `sgdNNN` rewound another copy of the same deployment. |
| #5226 | open | Rewind can move the block pointer forward. |
| #5741, #5521 | open | Rewind to start block does not work. |
| #5316 | open | Rewind causes a constraint violation that crashloops the indexer-agent. |
| #5607 | open | Failed with a non-deterministic error and not rewindable. |
| #5178 | open | Rewind is slow across many subgraphs. |

**Where waxwing can win.** Restoring a sealed layer is a rewind that never
mutates the live deployment: restore beside it, let it catch up, compare,
switch. It depends on the restore-to-block work in section 1.

## 4. Subgraphs stall silently

| Source | Complaint |
| --- | --- |
| Discord, 3-5 Oct 2026 | On v0.45.0, 2-3 of about 12 subgraphs on one index node stop indexing at random, not paused, healthy. `graphman restart` clears it until the next one. Three operators report it; nobody knows which version introduced it. |
| #6689 (open) | A Firehose stream that stays open but sends nothing hangs the subgraph forever: healthy, not paused, zero logs. No idle timeout on the receive loop. Present from v0.42.1 to master. |
| #6227 (open) | `graphman reassign` leaves the subgraph stuck about half the time. |
| #5253 (open) | Reassign should pause first. |
| #6382 (open) | Duplicate blocks in the cache silently block indexing. |

**Where waxwing cannot help.** This is the subgraph runner and the block
stream, not data movement. A watchdog that notices a frozen `latestBlock`
on a healthy deployment and restarts it would be useful and small, but it is
a different tool. The Discord reports and #6689 share a symptom; whether they
share a cause is not established, since #6689 is Firehose-specific and the
Discord thread does not say what the operators run.

## 5. What graphman dump gets wrong that nobody has reported

Found here, not in the tracker:

- An incremental dump after a reorg keeps the reverted rows. Reproduced by
  `rig/reorg.sh`: the restored copy has an entity that never existed on the
  canonical chain, two live versions of the same entity, and a diverging POI.
  The POI at the dump head still matches, so a head-only check passes.
- `docs/dump.md` does not document clamp files.
- Restore needs `graphman create <name>` first and the manifest on IPFS.

The first of these is worth an upstream issue.

## Reading

The complaints cluster on three things graphman does not offer: moving data
without rebuilding it, checking one copy against another, and going back
without risk. All three are operations on a sealed, versioned artefact rather
than on a live database, which is the position waxwing already occupies.
