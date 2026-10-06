use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use waxwing::{
    Attestation, Chain, Claim, CutAt, RestoreOptions, RpcChain, RpcStaking, SealOptions, Staking,
    apply_indexes, attest, cut, cut_block, diff, index_plan, ipfs_add, ipfs_cat, is_cid, is_final,
    read_catalogue, restore, seal, state, tally_claims, verify,
};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Hash a `graphman dump` directory and write catalogue.json into it
    Seal {
        dir: PathBuf,
        /// graph-node version that produced the dump
        #[arg(long)]
        graph_node_version: Option<String>,
        /// Public POI at the dump's head block
        #[arg(long)]
        public_poi: Option<String>,
        /// Ethereum JSON-RPC endpoint; refuse if any dump was taken on a
        /// fork this chain has reverted
        #[arg(long)]
        rpc: Option<String>,
        /// Do not record the state root, which reads every row
        #[arg(long)]
        no_state: bool,
    },
    /// Sign the state root of your own dump of a deployment, as of a block
    Attest {
        dir: PathBuf,
        /// File holding a hex secp256k1 private key, such as an operator key
        #[arg(long)]
        key_file: PathBuf,
        /// A block number, or `final`; the dump head when omitted
        #[arg(long)]
        at: Option<String>,
        /// Ethereum JSON-RPC endpoint, needed for a block below the head
        #[arg(long)]
        rpc: Option<String>,
        /// The indexer you sign for, when the key is its operator's
        #[arg(long)]
        indexer: Option<String>,
        /// Commit to history from this block on, to match a sealed dump
        /// that starts later than yours; your earliest block when omitted
        #[arg(long)]
        from: Option<i32>,
        /// Also put it on the Ethereum Attestation Service through this
        /// Arbitrum One endpoint, sent with --key-file, where `attested
        /// --eas` finds it by deployment. Costs a transaction's gas
        #[arg(long)]
        eas: Option<String>,
        /// Also add the attestation to IPFS through this Kubo API, and
        /// print its CID
        #[arg(long)]
        publish: Option<String>,
    },
    /// Count the indexers whose attestations agree with a sealed dump
    Attested {
        dir: PathBuf,
        /// Attestation files as written by `attest`, or their IPFS CIDs
        attestations: Vec<String>,
        /// Also count every attestation of the deployment on the Ethereum
        /// Attestation Service, read through this Arbitrum One endpoint
        #[arg(long)]
        eas: Option<String>,
        /// Arbitrum One block to search EAS from
        #[arg(long, default_value_t = waxwing::eas::SINCE)]
        since: u64,
        /// Count only these indexers; repeat for each
        #[arg(long, alias = "signer")]
        indexer: Vec<String>,
        /// Arbitrum One JSON-RPC endpoint: count only indexers with stake on
        /// the subgraph service, signing themselves or through an operator
        /// they authorised. Without it any key counts, which proves nothing
        /// about who signed
        #[arg(long)]
        network_rpc: Option<String>,
        /// Kubo API to read CIDs from
        #[arg(long)]
        ipfs: Option<String>,
        /// Fail unless at least this many distinct indexers agree
        #[arg(long, default_value_t = 1)]
        threshold: usize,
    },
    /// Re-hash a sealed dump directory against its catalogue.json
    Verify {
        dir: PathBuf,
        /// Ethereum JSON-RPC endpoint; also check that every dump was taken
        /// on this chain
        #[arg(long)]
        rpc: Option<String>,
        /// Fail unless the head is at or below the chain's finalized block
        #[arg(long, requires = "rpc")]
        require_final: bool,
    },
    /// Find the first block at which two dumps of one deployment disagree
    Diff {
        a: PathBuf,
        b: PathBuf,
        /// Compare as of this block rather than the lower of the two heads
        #[arg(long)]
        at: Option<i32>,
    },
    /// Write the dump a deployment would have had at an earlier block;
    /// restoring it is a rewind that never touches the source
    Cut {
        src: PathBuf,
        /// Must be empty or absent
        dst: PathBuf,
        /// A block number, or `final` for the chain's finalized block
        #[arg(long)]
        at: String,
        /// Ethereum JSON-RPC endpoint: checks the source is on this chain
        /// and learns the block's hash. Not needed when the block is one
        /// the source was sealed at
        #[arg(long)]
        rpc: Option<String>,
    },
    /// Give the deployment restored from a dump the indexes the dump
    /// recorded, not graphman's defaults. Prints the plan; changes nothing
    /// without --apply
    Indexes {
        dir: PathBuf,
        /// The receiving database, as postgresql://user:password@host/db
        #[arg(long)]
        db: String,
        /// The primary database, where graph-node's catalogue of deployments
        /// lives, if not the same as --db
        #[arg(long)]
        primary_db: Option<String>,
        /// The deployment's namespace (sgdN), where the database has several
        /// copies
        #[arg(long)]
        namespace: Option<String>,
        /// Build and drop indexes, concurrently, then stop graph-node adding
        /// postponed ones back
        #[arg(long)]
        apply: bool,
    },
    /// Restore a dump with its own indexes, built after the rows are loaded.
    /// graphman creates the deployment, parked on a node that does not
    /// exist; waxwing loads it and hands it to --node
    Restore {
        dir: PathBuf,
        /// The shard database to restore into, as postgresql://user:password@host/db
        #[arg(long)]
        db: String,
        /// The primary database, if not the same as --db
        #[arg(long)]
        primary_db: Option<String>,
        /// Your graphman config
        #[arg(long)]
        config: PathBuf,
        /// How to run graphman, e.g. "docker exec -i graph-node graphman"
        #[arg(long, default_value = "graphman")]
        graphman: String,
        /// An empty directory for waxwing's skeleton dump and graphman config
        #[arg(long)]
        work: PathBuf,
        /// Where graphman sees --work, if not at the same path
        #[arg(long)]
        work_as: Option<String>,
        /// The subgraph name, already created with `graphman create`
        #[arg(long)]
        name: String,
        #[arg(long, default_value = "primary")]
        shard: String,
        /// The node to index the deployment once restored
        #[arg(long)]
        node: String,
    },
    /// Hash a dump's entity versions as of a block, independent of vid,
    /// row order and encoding: what two indexers should agree on
    State {
        dir: PathBuf,
        /// Hash the state as of this block rather than the dump head
        #[arg(long)]
        at: Option<i32>,
        /// Leave out history before this block, as pruning to it would;
        /// the dump's earliest block when omitted
        #[arg(long)]
        from: Option<i32>,
    },
}

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Seal {
            dir,
            graph_node_version,
            public_poi,
            rpc,
            no_state,
        } => {
            let chain = rpc.map(RpcChain::new);
            let catalogue = seal(
                &dir,
                SealOptions {
                    graph_node_version,
                    public_poi,
                    chain: chain.as_ref().map(|c| c as &dyn Chain),
                    state: !no_state,
                },
            )?;
            let bytes: u64 = catalogue.files.iter().map(|f| f.bytes).sum();
            println!(
                "sealed {} at block {} ({}): {} files, {} bytes",
                catalogue.deployment,
                catalogue.head_block.number,
                catalogue.head_block.hash,
                catalogue.files.len(),
                bytes
            );
            if let Some(state_root) = &catalogue.state_root {
                println!("state {state_root}");
            }
            println!("root {}", catalogue.root);
            Ok(ExitCode::SUCCESS)
        }
        Command::Attest {
            dir,
            key_file,
            at,
            rpc,
            indexer,
            publish,
            from,
            eas,
        } => {
            let at = match at.as_deref() {
                None => None,
                Some("final") => Some(CutAt::Final),
                Some(number) => Some(CutAt::Block(
                    number
                        .parse()
                        .context("--at takes a block number or `final`")?,
                )),
            };
            let chain = rpc.map(RpcChain::new);
            let chain = chain.as_ref().map(|c| c as &dyn Chain);
            let state = match at {
                Some(at) => state(&dir, from, Some(cut_block(&dir, at, chain)?.number))?,
                None => state(&dir, from, None)?,
            };
            let block = cut_block(&dir, CutAt::Block(state.block), chain).or_else(|e| {
                // The head's hash is in the dump itself; anything lower is not.
                let head = waxwing::dump_head(&dir)?;
                if head.number == state.block {
                    Ok(head)
                } else {
                    Err(e)
                }
            })?;
            let key = std::fs::read_to_string(&key_file)
                .with_context(|| format!("reading {}", key_file.display()))?;
            let attestation = attest(
                &key,
                &state.deployment,
                &block,
                state.from,
                &state.root,
                indexer.as_deref(),
            )?;
            let json = serde_json::to_string_pretty(&attestation)?;
            println!("{json}");
            if let Some(url) = eas {
                eprintln!(
                    "on EAS as {}",
                    waxwing::eas::publish(&url, &key, &attestation)?
                );
            }
            if let Some(api) = publish {
                let name = format!(
                    "{}-{}.json",
                    attestation.deployment, attestation.block.number
                );
                eprintln!("published {}", ipfs_add(&api, &name, json.as_bytes())?);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Attested {
            dir,
            attestations,
            eas,
            since,
            indexer,
            network_rpc,
            ipfs,
            threshold,
        } => {
            let catalogue = read_catalogue(&dir)?;
            let state_root = catalogue
                .state_root
                .context("the catalogue records no state root: seal without --no-state")?;
            let expected = Attestation {
                version: 2,
                deployment: catalogue.deployment,
                block: catalogue.head_block,
                from: catalogue.earliest_block_number,
                state_root,
                indexer: None,
                signer: String::new(),
                signature: String::new(),
            };
            let mut read = Vec::new();
            for source in &attestations {
                let raw = if is_cid(source) && !std::path::Path::new(source).exists() {
                    let api = ipfs.as_deref().context("--ipfs is needed to read a CID")?;
                    ipfs_cat(api, source)?
                } else {
                    std::fs::read(source).with_context(|| format!("reading {source}"))?
                };
                let attestation: Attestation =
                    serde_json::from_slice(&raw).with_context(|| format!("parsing {source}"))?;
                read.push(attestation);
            }
            if read.is_empty() && eas.is_none() {
                anyhow::bail!("give attestation files, CIDs, or --eas to find them on chain");
            }
            let mut claims: Vec<Claim> = read
                .iter()
                .filter_map(|a| {
                    Some(Claim {
                        signer: a.recover().ok()?,
                        attestation: a.clone(),
                    })
                })
                .collect();
            if let Some(url) = &eas {
                let found = waxwing::eas::find(url, &expected.deployment, since)?;
                println!(
                    "found {} attestation(s) of {} on EAS",
                    found.len(),
                    expected.deployment
                );
                read.extend(found.iter().map(|c| c.attestation.clone()));
                claims.extend(found);
            }
            let staking = network_rpc.map(RpcStaking::new);
            let agreed = tally_claims(
                &expected,
                &claims,
                &indexer,
                staking.as_ref().map(|s| s as &dyn Staking),
            )?;
            for agreement in &agreed {
                match (agreement.stake, agreement.signer == agreement.indexer) {
                    (Some(stake), true) => println!(
                        "agrees: indexer {} with {} GRT available",
                        agreement.indexer,
                        stake / 10u128.pow(18)
                    ),
                    (Some(stake), false) => println!(
                        "agrees: indexer {} with {} GRT available, signed by its operator {}",
                        agreement.indexer,
                        stake / 10u128.pow(18),
                        agreement.signer
                    ),
                    (None, _) => println!("agrees: {}", agreement.signer),
                }
            }
            println!(
                "{} of {} attestation(s) agree on state {} at block {}, for {} indexer(s); need {threshold}",
                read.iter().filter(|a| a.agrees_with(&expected)).count(),
                read.len(),
                expected.state_root,
                expected.block.number,
                agreed.len()
            );
            if staking.is_none() {
                println!(
                    "no --network-rpc given: any key counts, so this shows agreement, not who agrees"
                );
            }
            Ok(if agreed.len() >= threshold {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
        Command::Cut { src, dst, at, rpc } => {
            let at = match at.as_str() {
                "final" => CutAt::Final,
                number => CutAt::Block(
                    number
                        .parse()
                        .context("--at takes a block number or `final`")?,
                ),
            };
            let chain = rpc.map(RpcChain::new);
            let block = cut_block(&src, at, chain.as_ref().map(|c| c as &dyn Chain))?;
            let result = cut(&src, &dst, block)?;
            println!(
                "cut at block {} ({}): {} versions, {} dropped, {} reopened",
                result.block.number,
                result.block.hash,
                result.versions,
                result.dropped,
                result.reopened
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::State { dir, at, from } => {
            let state = state(&dir, from, at)?;
            for table in &state.tables {
                println!(
                    "{:<20} {} {} versions",
                    table.table, table.root, table.versions
                );
            }
            println!(
                "state {} at block {} from {}: {}",
                state.deployment, state.block, state.from, state.root
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Indexes {
            dir,
            db,
            primary_db,
            namespace,
            apply,
        } => {
            let mut db = postgres::Client::connect(&db, postgres::NoTls)
                .context("connecting to the database")?;
            let mut primary = match primary_db {
                Some(url) => Some(
                    postgres::Client::connect(&url, postgres::NoTls)
                        .context("connecting to the primary database")?,
                ),
                None => None,
            };
            let plan = index_plan(&dir, &mut db, primary.as_mut(), namespace.as_deref())?;
            println!(
                "{}: {} to build, {} to drop",
                plan.namespace,
                plan.create.len(),
                plan.drop.len()
            );
            if !apply {
                for sql in plan.create.iter().chain(&plan.drop) {
                    println!("{sql};");
                }
                return Ok(ExitCode::SUCCESS);
            }
            apply_indexes(&plan, &mut db, |sql| println!("{sql};"))?;
            println!("done");
            Ok(ExitCode::SUCCESS)
        }
        Command::Restore {
            dir,
            db,
            primary_db,
            config,
            graphman,
            work,
            work_as,
            name,
            shard,
            node,
        } => {
            let options = RestoreOptions {
                db,
                primary_db,
                graphman: graphman.split_whitespace().map(String::from).collect(),
                config,
                work,
                work_as,
                name,
                shard,
                node,
            };
            let start = std::time::Instant::now();
            restore(&dir, &options, |step| {
                println!("{:>6.1}s {step}", start.elapsed().as_secs_f64())
            })?;
            println!("restored in {:.1}s", start.elapsed().as_secs_f64());
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff { a, b, at } => {
            let diff = diff(&a, &b, at)?;
            match diff.from {
                0 => println!("comparing {} as of block {}", diff.deployment, diff.block),
                from => println!(
                    "comparing {} as of block {}, from block {from} on",
                    diff.deployment, diff.block
                ),
            }
            for table in &diff.tables {
                let Some(first) = table.first_block else {
                    println!(
                        "{:<20} identical, {} versions",
                        table.table, table.versions[0]
                    );
                    continue;
                };
                println!(
                    "{:<20} DIVERGES at block {first}: {} only in A, {} only in B, {} closed differently",
                    table.table, table.only[0], table.only[1], table.closed_differently
                );
                for sample in &table.samples {
                    println!("    {sample}");
                }
            }
            match diff.first() {
                None => {
                    println!("identical");
                    Ok(ExitCode::SUCCESS)
                }
                Some((block, table)) => {
                    println!("first divergence at block {block}, in {table}");
                    Ok(ExitCode::FAILURE)
                }
            }
        }
        Command::Verify {
            dir,
            rpc,
            require_final,
        } => {
            let chain = rpc.map(RpcChain::new);
            let chain = chain.as_ref().map(|c| c as &dyn Chain);
            let (catalogue, problems) = verify(&dir, chain)?;
            let mut not_final = false;
            if let Some(chain) = chain
                && problems.is_empty()
            {
                not_final = !is_final(&catalogue.head_block, chain)?;
                println!(
                    "head block {} is {} on this chain",
                    catalogue.head_block.number,
                    if not_final { "NOT yet final" } else { "final" }
                );
            }
            if not_final && require_final {
                eprintln!("a head that is not final can still be reverted");
                return Ok(ExitCode::FAILURE);
            }
            if problems.is_empty() {
                println!(
                    "ok {} at block {}: {} layer(s), {} files, root {}",
                    catalogue.deployment,
                    catalogue.head_block.number,
                    catalogue.layers.len(),
                    catalogue.files.len(),
                    catalogue.root
                );
                return Ok(ExitCode::SUCCESS);
            }
            for problem in &problems {
                eprintln!("{problem}");
            }
            eprintln!("{} problem(s)", problems.len());
            Ok(ExitCode::FAILURE)
        }
    }
}
