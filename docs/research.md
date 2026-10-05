# Sharing Indexed Subgraph Data Between Graph Indexers: State of Play and a Design

> The founding research note, kept as written. Its largest unknown, the
> `graphman dump` format, has since been read from the code; the corrections
> are in the [README](../README.md).

Nobody has shipped a production-grade way to hand a synced subgraph deployment from one indexer to another. The closest piece is graph-node's experimental `graphman dump`/`graphman restore`. My recommendation is a sidecar that turns the finalized part of a deployment's `sgdN` schema into sealed, content-addressed segments of entity-version changes. Trust comes from three things: cross-checking public POIs, multi-party segment attestation, and sampled re-execution. It should not rest on the POI alone, because the POI is a running hash over handler writes and does not commit to state. The weakest link is the trust model, not the transport: no existing mechanism lets a receiver prove a restored entity table is correct without re-executing at least part of the history.

## TL;DR

- **State of the art:** `graphman dump`/`restore` (graph-node v0.42.0, March 2026) is the only first-party mechanism. It is officially experimental. GraphOps' File Hosting Service (formerly Subfile/File Data Service) never reached production for subgraph data. Subgraph Radio only exchanges POIs and pre-sync signals, not data. I could not read the dump code or `docs/dump.md` directly (GitHub blocked automated access), so the on-disk format details below are inferred, not verified.
- **Recommended design:** a sidecar that tails `sgdN` (via logical decoding or windowed queries on a hot standby) and seals immutable Parquet segments past finality. Each segment holds row inserts plus `block_range` closes for entity tables, `data_sources$` and `poi2$`, and a catalogue records the deployment hash, final block/hash, graph-node version and the public POI at that block. This maps closely onto your existing sealed-segment/catalogue/publisher design. It can start as a pure sidecar; graph-node changes come later (deterministic canonical segment hashing, a "replay window and diff" mode, a restore-from-segments path).
- **Trust and economics are the open problems:** a matching POI at block F proves the publisher's *digest* matches consensus, not that the rows you received produce it. Serving queries or submitting POIs from restored data puts the slashing risk on you, and the Arbitration Charter has no provision for "I imported bad data". Indexing rewards are shared pro rata among allocated indexers, so sharing a sync with a competitor directly dilutes the sharer. A paid Horizon data service settled through GraphTally is plausible; nobody has built one for subgraph state.

## Key Findings

### What I verified from primary sources

**graph-node dump/restore and adjacent changes (release notes / NEWS.md):**
- v0.42.0 (released 19 March 2026) introduced "`graphman dump` and `graphman restore` (experimental). New commands for exporting and importing subgraph data in Parquet format, enabling backup and migration of deployment data across shards. Supports incremental dumps and progress reporting. See `docs/dump.md` for details. (#6397)".
- **This contradicts your reading that there is no incremental mode.** The release notes explicitly claim "incremental dumps". I could not read `docs/dump.md` or the code to find out what "incremental" means: resuming an interrupted dump, appending new block ranges to an existing dump directory, or something else. Treat this as unresolved and check `docs/dump.md` first.
- v0.45.0 (released 3 August 2026) fixed "`graphman dump` panicking on tables with more than ~2 GiB in a single string/binary column; batches are now split into byte-bounded slices before conversion to Arrow (#6646)". This confirms the dump pipeline goes Postgres → Arrow record batches → Parquet.
- The same v0.42.0 release **removed Substreams support entirely** ("Substreams-based subgraphs will no longer work", #6261). It added **Amp-powered subgraphs (experimental)**, which "index data directly from Amp servers instead of processing blocks individually, reducing indexing time from days/weeks to minutes/hours".
- Deployment-movement bugs that any snapshot design must handle:
  - #6384: `graphman copy` copied a pruned `earliest_block_number`, leaving the destination invalid.
  - #6610 (v0.45.0): grafts silently succeeded below the base's earliest block, leaving missing pre-graft versions.
  - #6687 (v0.45.0): fulltext `tsvector` columns were silently dropped on copy/graft.
  - #6340 (v0.41.2): rewinding pruned subgraphs could corrupt entity state, leaving entities with no open version.
- **POI is version-sensitive.** The v0.44.0 upgrade note says subgraphs reading derived fields "may diverge in POI from v0.43.0 on blocks exhibiting this pattern… The v0.44.0 result is canonical." A snapshot produced by an older graph-node can therefore carry state that is now non-canonical.
- **Not all entities are covered by the POI.** v0.30.0 NEWS states that entity updates from file/offchain data source handlers "do **not** contribute to PoIs". I did not find a later change that reverses this; treat it as true unless the code says otherwise.
- **Non-determinism hazards that change classification:** v0.45.0 #6645 fixed Reth EVM halts being misclassified as non-deterministic, and 0.25.1 fixed retries after non-deterministic errors that "could make the POI generation non-deterministic".

**POI and disputes (official docs, GIPs):**
- The Graph docs define a POI for a block as "a digest of all entity store transactions for a specific Subgraph deployment up to and including that block". Under Horizon, POIs are submitted periodically, and a POI older than `maxPOIStaleness` (28 days) lets anyone force-close the allocation.
- The docs also say indexers "are recommended to utilize offchain syncing functionality to sync Subgraph deployments to chainhead before creating the allocation onchain". This is the natural slot for snapshot bootstrap.
- Deterministic failures are "final", with a POI generated for the failing block. Non-deterministic failures are not final.
- The network-design blog ("The Graph Network In Depth – Part 2") describes the POI as "a signature over a message digest", specific to each indexer. It lists Merkle trees or polynomial commitments as *future work*. That is still future work today.
- Under Horizon, slashing parameters are "flexible, with a maximum cap of 10% (the recommended default is 2.5%)".
- The original Arbitration Charter (GIP-0009) is relevant here in five ways:
  - Arbitrators decide by "reproducing the work themselves".
  - Determinism bugs are encouraged to resolve as Draw.
  - Query slashing is capped at "once per epoch per allocation".
  - There is a statute of limitations of two thawing periods.
  - PoIs that are correct only for the previous official software version are settled as Draw.
  - The Horizon-era charter is GIP-0085; I did not read it.

**Grafting (docs, engineering plan, forum):**
- Grafting copies the base deployment's data up to a block and continues indexing from there. The base "must exist on the target Graph Node instance and must have indexed up to at least the given block". Older official docs advised against grafting on the network for exactly this reason.
- The forum thread on grafting in the decentralised network states that there is one valid POI for a grafted subgraph: index the base up to block X, then index the subgraph as defined. A new indexer therefore has to index the whole graft chain.

**GraphOps File Hosting Service (repo, office hours, forum updates):**
- The `graphops/file-hosting-service` repo (still reachable at the old `subfile-data-service` URL) describes a P2P, payments-enabled file marketplace. It uses IPFS-published hash commitments, chunked SHA2-256 verification and HTTP2.
- Micropayments are still listed under "To be supported". The repo has 189 commits, 17 open issues and 4 stars on the `dev` branch.
- In office hours #134 (Pinax write-up), GraphOps' Kara was described as working on "a file data service allowing indexers to store and serve flat files, such as Ethereum data", which "will use manifests to describe the files", verifies by "a list of hashes for the file chunks", and lets clients request chunks from multiple indexers. Office hours #140 (Pinax write-up, 19 January 2024) had GraphOps' Hope presenting it as a way to "reduce redundant work and boot-strapping costs".
- The GraphOps March 2024 update reported it was ported to the indexer-rs framework, with "read/write object storage" and "testing with initial users" as next steps.
- I found no formal sunset announcement. The latest signal I found is Indexer Office Hours #174 (Pinax write-up dated 13 September 2024), where Alexis, asked whether more services were coming, said: "I think it's way down the line, as some things have been deprioritized in terms of building an indexer service framework for new services".
- **Conclusion:** stalled, not formally killed. It was aimed at flat files (Firehose/blobs), not at Postgres subgraph state.

**Subgraph Radio / Graphcast:** it offers POI cross-checking and "Subgraph Upgrade Pre-sync" (developers announce a new deployment so indexers start syncing early). Version 1.0.1 shipped in March 2024. POI Radio was archived in July 2023 and rebranded as Subgraph Radio. A GraphOps description lists "facilitating rapid sync negotiations… to bootstrap their initial dataset, across subgraphs, Substreams flat files and Firehose flat files" as a *possible* Graphcast use. I found no evidence it was built.

**Horizon and payments:**
- Horizon went live on 11 December 2025 (date per secondary coverage such as Cryptowisser; I did not fetch The Graph's own announcement). Per the 2026 technical roadmap it has a core staking protocol, unified payments and "a framework for permissionless data service development".
- The roadmap lists for Q2 2026 a "Substreams MVP data service with GraphTally trust-minimized payments, Horizon-based P2P data service introduced".
- The Horizon docs say the payments protocol is a generalised TAP v1 and integrates GraphTally "but also any other arbitrary payment collection system".
- A community RFC (GRC-006 "Mainline", a Firehose data service) shows the pattern for a non-subgraph Horizon service settled via GraphTally, `GraphTallyCollector` and `PaymentsEscrow`.
- The 2024 "New Era" roadmap promised "a dynamic file sharing marketplace for Indexers".

### What I could not verify (thin evidence, stated plainly)

- **The dump code and `docs/dump.md`.** GitHub blocked automated access to the docs directory, and the PR #6397/#6646 pages never came up in search; a research subagent hit the same wall. I therefore **cannot confirm** any of the following:
  - how `block_range`/`vid`/`causality_region` are encoded;
  - whether `data_sources$`, `poi2$`, the `subgraphs.head`/`subgraphs.deployment` metadata, graft base or `history_blocks` are included;
  - what restore validates.
  - Since v0.39.0 the old `subgraph_deployment` table is split into `subgraphs.head` and `subgraphs.deployment`, so look for those names in the code.
- Your recollections that `graphman copy` is intra-installation only and that operators pass around `pg_dump`s of `sgdN` schemas: I found no primary source either way. The informal `pg_dump` practice is plausible but undocumented.
- Ponder, SQD and Goldsky bootstrap/snapshot mechanisms: I did not research these in this pass, so I make no claims about them. For Envio, I found only that HyperSync accelerates raw data retrieval; I found nothing about sharing handler output.
- Whether `vid` is deterministic across installations. I believe newer graph-node derives `vid` from block number and an in-block sequence, but I did not verify this. It matters for canonical hashing (see below).
- The exact POI digest construction: per-causality-region digests, and whether intra-block overwritten writes are hashed. My understanding comes from prior familiarity with the code, not from this session.

## Details

### 1. What dump/restore gives you today and why it is not the answer

From the verified facts, dump/restore is a Parquet export/import of a deployment through Arrow, marketed for "backup and migration… across shards". It is experimental, may have some incremental capability, and is unverified with respect to POI. Even if it already captures `data_sources$` and `poi2$`, it lacks four things a cross-operator handoff needs:
- a published identity (a content hash of the artefact);
- a declared final block and block hash;
- a binding to graph-node version and POI;
- a non-disruptive export path. The single-transaction dump you noted is a real problem on a primary; running it against a hot standby is the obvious workaround.

The index-rebuild cost on restore is also unknown. A related `graphman copy` issue (#6722) reports that copy now builds indexes from the default layout, adding "+1.35 TB on a 1.6 TB deployment". If restore behaves similarly, index build time could dominate restore time for the very large subgraphs you care about.

### 2. Neighbouring approaches: do any make the problem moot?

- **Substreams-powered subgraphs:** removed from graph-node in v0.42.0. They are no longer an escape hatch inside graph-node.
- **Firehose flat files (and the Mainline RFC):** they remove the RPC/extraction bottleneck and are easy to share, because they are immutable and content-addressable. WASM handler execution and Postgres writes still have to happen, and for the weeks-long subgraphs that is usually the cost. Partial help only.
- **Amp-powered subgraphs:** the one development that genuinely moots the problem, but only for subgraphs *rewritten* as Amp subgraphs. They are a different deployment hash, still experimental, and tied to Amp servers (an Edge & Node commercial product). Existing WASM deployments with weeks of history get nothing.
- **Grafting:** it is local copy-on-start and needs the base on the same node, so it adds to the problem rather than solving it. A snapshot system would actually *fix* grafting on the network: a grafted deployment's snapshot already contains the copied base data, so a consumer never needs the base.
- **Upgrade Indexer / Subgraph Radio pre-sync:** these shorten time-to-availability by starting early. Nothing is transferred.

### 3. The trust problem

**What the POI commits to.** It is a running digest over the sequence of entity modifications made by handlers, block by block, and it is signed/salted per indexer. The docs describe it as a digest "of all entity store transactions". It is **not** a commitment to the resulting table state. Three consequences (inferred from that construction):
1. You cannot recompute the POI from a restored snapshot without re-executing handlers. The final rows do not determine the write sequence; intra-block overwrites, for example, are likely hashed but never stored.
2. If the snapshot includes `poi2$`, you can take the digest at block F, derive the *public* POI (zero indexer address), and compare it with public POIs from other indexers via the index-node API or Subgraph Radio. A match proves only that the publisher's stored digest equals consensus.
3. A malicious or buggy publisher could ship a correct `poi2$` with corrupted entity rows. Your POIs would keep matching consensus until a handler *reads* a corrupted entity, at which point your POI diverges. Corruption in entities that no handler ever reads again (immutable or terminal state) is never detected by POI monitoring. It surfaces only through query cross-checks.

**File data source entities are outside the POI entirely** (v0.30.0 NEWS). They can only be verified by re-fetching from IPFS and re-running those handlers.

**What actually gives verification:**
- **Public POI cross-check at F** against k independent indexers. This is cheap and binds the snapshot's digest to consensus.
- **Multi-party segment attestation:** independent indexers who synced themselves publish hashes of the *canonical* segment for each finalized window. Agreement among k-of-N independent parties is evidence about rows, not just digests. It requires a deterministic canonical encoding: sort by (table, id, lower block), exclude `vid` unless it is provably deterministic, and normalise numeric/bytes encodings.
- **Sampled re-execution:** unpruned graph-node keeps full version history, so the consumer can reconstruct state at any block k, re-run handlers for k..k+w, and diff the produced versions and POI digests against the snapshot. Sampling m random windows gives probabilistic fraud detection at a cost proportional to m·w rather than to the whole history.
- **Attested query cross-checks** against other indexers at historical blocks.

**What remains unverifiable without full re-indexing:** rows never re-read by handlers and never sampled; file data source entities unless re-fetched; and correctness relative to graph-node version changes (the v0.44.0 example).

**Slashing exposure.**
- Query responses are signed attestations. If restored data is wrong, you are slashable on query disputes: per the original charter at most once per epoch per allocation, and under Horizon up to the 10% cap per slash.
- Your POIs chain from the restored digest, so a wrong digest makes every subsequent POI wrong and disputable.
- The charter's Draw provisions cover *determinism bugs* and previous-version software. Nothing covers "the snapshot I bought was bad", so the risk sits with the consumer (my reading; GIP-0085 may differ).
- Practical rule: do not allocate until (a) the public POI at F matches consensus, (b) your own indexing from F has produced matching public POIs at several later blocks, and (c) query cross-checks pass.

### 4. Economics

- **Why a syncer would not share for free.** Indexing rewards on a subgraph are split pro rata among allocated stake, and query fees are competitive. Every competitor you bootstrap dilutes your rewards and query share. Free sharing only makes sense between affiliated operators, or for parties that benefit from more indexers on a subgraph: gateways, subgraph developers, and the Upgrade Indexer. Per The Graph's Quick Start docs, a Studio deployment's indexing "is performed by the Upgrade Indexer, which is a single Indexer owned and operated by Edge & Node" (a docs PR, #1124, rewords the operator as The Graph Foundation; its merge status is unchecked). Edge & Node's October 2023 "Introducing the Upgrade Indexer" post says it "will not collect indexing rewards or compete with other Indexers", which makes it the obvious neutral seed source.
- **A plausible market.** The buyer's willingness to pay is capped by their own cost of syncing: weeks of RPC/Firehose plus compute plus lost reward time. The seller's marginal cost is egress. That is a real spread.
- **Horizon fit.** Horizon already supplies a permissionless data-service contract, provisioned stake, and GraphTally receipts for per-chunk micropayments, which is exactly the FHS chunk-payment idea. A "Subgraph Snapshot Service" could require sellers to provision stake slashable for serving segments that fail attestation. That gives buyers recourse that the subgraph charter does not. None of this exists. The FHS precedent suggests the hard part is adoption and maintenance, not the protocol.
- **Price discovery is circular.** Sellers with the data are exactly the competitors who lose by selling it. Expect a monopoly-pricing problem on niche subgraphs, which multi-seller attestation partly mitigates.

## Recommendations: Competing Designs

### Design A (recommended): Finality-sealed change-log segments, sidecar first

**Artefact.** For each deployment, a catalogue JSON (installed atomically) plus immutable Parquet segments, one per finalized block window (B_i, B_{i+1}]. Each segment holds, per table (entity tables, `data_sources$`, `poi2$`):
- (a) **inserts:** full rows whose `lower(block_range)` falls in the window, or whose `block$` does for immutable entities;
- (b) **closes:** (table, id/vid, upper) for versions whose `upper(block_range)` falls in the window.

Once B_{i+1} ≤ the finality threshold, both sets are deterministic and never change, barring a rewind (which must invalidate the catalogue). This is the graph-node analogue of your sealed segments: the immutable unit is the version *event*, not the row, because rows are mutated when their range is closed.

The catalogue records: deployment hash, chain, graph-node version and apiVersion, layout/schema hash, earliest block, graft base and block, `history_blocks`, final block number and hash, public POI at that block, error state, and segment CIDs. Publishing is your reconciler unchanged: append-only, idempotent, level-triggered, outside the write path.

**Catch-up.** The consumer creates the deployment, bulk-loads inserts, applies closes in window order, sets head to F, rebuilds indexes (or defers them using v0.45.0's background postponed index creation), then lets graph-node index from F+1 to head.

**Sidecar vs graph-node changes.**
- Phase 1 is a pure sidecar. It reads a hot standby either through windowed `REPEATABLE READ` queries on `lower/upper(block_range)` or through Postgres logical decoding on the `sgdN` schema, buffering changes until they pass finality so reorg reverts cancel out.
- Restore can initially go through a custom loader or adapted `graphman restore`, writing `subgraphs.head`/`subgraphs.deployment` directly. That is fragile across graph-node migrations.
- Phase 2 graph-node changes:
  1. an official "restore from catalogue to block F" command that writes metadata and data-source state correctly;
  2. a canonical, deterministic segment hash, emitted optionally at write time from the batch writer, which already knows inserts and clamps;
  3. a "replay window k..k+w and diff" mode for sampled verification;
  4. rewind hooks that invalidate published windows.

**Edge cases.**
- *Pruned deployments:* a publisher can only serve from its earliest block. The catalogue must say so, and consumers inherit the pruning (no time travel, no grafting below it, cf. #6384/#6610). Verify whether `poi2$` survives pruning before relying on it.
- *Grafts:* publish the grafted deployment's own segments. The base is never needed downstream.
- *Dynamic data sources:* `data_sources$` rows (with causality region and creation block) are mandatory. A restore that omits them silently stops triggering handlers for factory-created contracts, which shows up as a POI divergence soon after F.
- *File data sources:* include them but mark them as POI-uncovered.
- *Deterministic failures:* publish up to the failure block plus the error record. Consumers can serve the final POI immediately.
- *Non-deterministic failures:* publish only up to the last final good block.
- *Version-divergent ranges* (v0.44.0-style): consumers reject catalogues whose graph-node version is flagged non-canonical for the range.

**Trust model.** Public POI cross-check at F, plus k-of-N segment attestation once multiple publishers exist, plus sampled replay. Unverifiable residue: unread, unsampled rows and file data source entities.

**Smallest useful v1.** One-shot export at final block F from a standby, using `graphman dump` if its format turns out adequate, otherwise a custom exporter. Wrap it in the catalogue with CIDs and the public POI at F, upload it to a bucket, and restore on a second installation. No incremental windows yet.

**Kill experiments (run these first):**
1. Restore three large deployments, chosen for factories/dynamic data sources, account-like tables and fulltext, then index 10k+ blocks. Public POIs at F+N must match the network. If they diverge, there is hidden state outside the tables (sequence counters, data-source ordering, entity-cache assumptions) and the sidecar approach needs graph-node changes from day one.
2. Measure restore-plus-index-build time per TB against sync time. If restoring a 1 TB deployment takes days because of index builds, the advantage shrinks to "days vs weeks".
3. Hash the same finalized window twice, a week apart, on a live deployment. Any change means the immutability assumption past finality is wrong, for example because of pruning or account-like optimisation rewriting rows.

### Design B: Periodic full snapshots plus multi-indexer attestation (simpler, coarser)

**Artefact.** A full deployment snapshot at final block F, taken every N days from a standby, with the same catalogue. Several independent indexers publish the canonical hash of the snapshot of the same deployment at the same F (agreed epochs, e.g. epoch start blocks).

**Catch-up.** Restore the latest snapshot, then index from F. Lag is bounded by N days of indexing.

**Build.** Sidecar only, using `graphman dump` plus canonicalisation. Requires the deterministic canonical encoding to be agreed by publishers.

**Edge cases.** Same as A, but simpler because there are no close events. Storage and egress cost is O(size) per snapshot.

**Trust.** The strongest attestation story for the least engineering: k independent indexers agreeing on a state hash at F is a genuine state commitment, which the POI is not. Unverifiable: collusion, and correlated graph-node bugs (all honest indexers wrong together, which the charter would treat as a determinism bug).

**v1 / kill test.** Get two independent indexers to produce byte-identical canonical exports of one deployment at the same F. If they cannot, because of `vid`, float or numeric encoding, or ordering, multi-party attestation in both A and B needs canonicalisation work first.

### Design C: Horizon "Snapshot Data Service" (market layer on top of A or B)

**Artefact.** A or B segments, served by staked providers. Payment is per chunk via GraphTally receipts, and providers are slashable if a served segment's hash fails to match the k-of-N attested hash or fails sampled replay.

**Build.** A Horizon data service contract (following the Mainline/GRC-006 pattern), an indexer-rs-based server, and a client in the indexer-agent that bootstraps "offchain sync" from purchased segments.

**Trust.** It adds economic recourse for buyers, which the subgraph Arbitration Charter does not offer. The arbitration rules for "bad segment" disputes would need to be written.

**v1 / kill test.** Before writing contracts, run Design B among three or four cooperating indexers for one quarter and track whether anyone would pay. FHS shows that well-engineered infrastructure without committed users stalls.

### Bottom line

Build Design A's v1 (one-shot export plus catalogue plus restore) and run the three kill experiments within weeks. If POIs match after restore, add B-style attestation, because it is the only route to an actual state commitment. Only then consider C. Before writing any code, read `docs/dump.md` and the #6397 diff to settle what "incremental dumps" means and what restore already writes. That single unknown could make A's exporter largely free, or show that `graphman dump` misses `data_sources$`/`poi2$` and cannot be reused.

## Caveats

- I could not inspect the graph-node dump/restore code or its documentation. Every statement about the dump format is inference, and the release notes' "incremental dumps" claim conflicts with your reading of the PR.
- The POI-construction details (per-region digests, hashing of intra-block overwrites) and the `vid` determinism assumption come from my understanding of graph-node, not from code read in this session.
- I did not research Ponder, SQD or Goldsky. I found no formal sunset notice for the File Hosting Service; "stalled" is my reading of the last public signals in 2024.
- The economics section is reasoning about incentives, not observed market data. No snapshot market exists to measure.
