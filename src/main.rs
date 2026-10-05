use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use waxwing::{
    Chain, CutAt, RpcChain, SealOptions, cut, cut_block, diff, is_final, seal, state, verify,
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
    /// Hash a dump's entity versions as of a block, independent of vid,
    /// row order and encoding: what two indexers should agree on
    State {
        dir: PathBuf,
        /// Hash the state as of this block rather than the dump head
        #[arg(long)]
        at: Option<i32>,
    },
}

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Seal {
            dir,
            graph_node_version,
            public_poi,
            rpc,
        } => {
            let chain = rpc.map(RpcChain::new);
            let catalogue = seal(
                &dir,
                SealOptions {
                    graph_node_version,
                    public_poi,
                    chain: chain.as_ref().map(|c| c as &dyn Chain),
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
            println!("root {}", catalogue.root);
            Ok(ExitCode::SUCCESS)
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
        Command::State { dir, at } => {
            let state = state(&dir, at)?;
            for table in &state.tables {
                println!(
                    "{:<20} {} {} versions",
                    table.table, table.root, table.versions
                );
            }
            println!(
                "state {} at block {}: {}",
                state.deployment, state.block, state.root
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Diff { a, b, at } => {
            let diff = diff(&a, &b, at)?;
            println!("comparing {} as of block {}", diff.deployment, diff.block);
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
