use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use waxwing::{Chain, RpcChain, SealOptions, seal, verify};

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
        Command::Verify { dir, rpc } => {
            let chain = rpc.map(RpcChain::new);
            let (catalogue, problems) = verify(&dir, chain.as_ref().map(|c| c as &dyn Chain))?;
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
