//! The Graph network as an attestation's referee: whether an address is an
//! indexer with stake behind the subgraph service, or an operator one has
//! authorised. And IPFS, as somewhere to put attestations.

use std::io::Read as _;

use anyhow::{Context, Result, bail};
use serde_json::json;
use sha3::{Digest, Keccak256};

/// Graph Horizon on Arbitrum One.
pub const STAKING: &str = "0x00669a4cf01450b64e8a2a20e9b1fcb71e61ef03";
pub const SUBGRAPH_SERVICE: &str = "0xb2bb92d0de618878e438b55d5846cfecd9301105";

pub trait Staking {
    /// Tokens the indexer has provisioned to the subgraph service and is
    /// not thawing.
    fn tokens_available(&self, indexer: &str) -> Result<u128>;
    /// Whether the indexer lets `operator` act for it on the subgraph
    /// service.
    fn is_authorized(&self, indexer: &str, operator: &str) -> Result<bool>;
}

pub struct RpcStaking {
    url: String,
    staking: String,
    service: String,
}

impl RpcStaking {
    pub fn new(url: impl Into<String>) -> Self {
        Self::with_contracts(url, STAKING, SUBGRAPH_SERVICE)
    }

    pub fn with_contracts(url: impl Into<String>, staking: &str, service: &str) -> Self {
        Self {
            url: url.into(),
            staking: staking.to_string(),
            service: service.to_string(),
        }
    }

    fn call(&self, signature: &str, addresses: &[&str]) -> Result<Vec<u8>> {
        let mut data = Keccak256::digest(signature)[..4].to_vec();
        for address in addresses {
            data.extend_from_slice(&word(address)?);
        }
        let response: serde_json::Value = ureq::post(&self.url)
            .send_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_call",
                "params": [{ "to": self.staking, "data": format!("0x{}", hex::encode(data)) }, "latest"],
            }))
            .with_context(|| format!("calling {signature} on {}", self.url))?
            .into_json()?;
        if let Some(error) = response.get("error") {
            bail!("{} refused {signature}: {error}", self.url);
        }
        let result = response["result"]
            .as_str()
            .with_context(|| format!("{} returned no result for {signature}", self.url))?;
        let bytes = hex::decode(result.trim_start_matches("0x"))?;
        if bytes.len() != 32 {
            bail!("{signature} returned {} bytes, not 32", bytes.len());
        }
        Ok(bytes)
    }
}

impl Staking for RpcStaking {
    fn tokens_available(&self, indexer: &str) -> Result<u128> {
        let word = self.call(
            "getProviderTokensAvailable(address,address)",
            &[indexer, &self.service],
        )?;
        // More GRT than exists; not worth a wider integer.
        if word[..16].iter().any(|b| *b != 0) {
            return Ok(u128::MAX);
        }
        Ok(u128::from_be_bytes(word[16..].try_into().unwrap()))
    }

    fn is_authorized(&self, indexer: &str, operator: &str) -> Result<bool> {
        let word = self.call(
            "isAuthorized(address,address,address)",
            &[indexer, &self.service, operator],
        )?;
        Ok(word[31] == 1)
    }
}

fn word(address: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(address.trim_start_matches("0x"))
        .with_context(|| format!("{address} is not an address"))?;
    if bytes.len() != 20 {
        bail!("{address} is not an address");
    }
    let mut word = [0; 32];
    word[12..].copy_from_slice(&bytes);
    Ok(word)
}

/// Whether `s` reads as an IPFS CID rather than a path.
pub fn is_cid(s: &str) -> bool {
    (s.starts_with("Qm") && s.len() == 46) || (s.starts_with("baf") && s.len() > 50)
}

/// Add `bytes` to IPFS through a Kubo API, pinned, and return the CID.
pub fn ipfs_add(api: &str, name: &str, bytes: &[u8]) -> Result<String> {
    let boundary = "waxwing-attestation-boundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/json\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let url = format!("{}/api/v0/add?pin=true", api.trim_end_matches('/'));
    let response: serde_json::Value = ureq::post(&url)
        .set(
            "Content-Type",
            &format!("multipart/form-data; boundary={boundary}"),
        )
        .send_bytes(&body)
        .with_context(|| format!("adding to IPFS at {api}"))?
        .into_json()?;
    response["Hash"]
        .as_str()
        .map(str::to_string)
        .with_context(|| format!("{api} returned no CID: {response}"))
}

/// Read a CID through a Kubo API.
pub fn ipfs_cat(api: &str, cid: &str) -> Result<Vec<u8>> {
    let url = format!("{}/api/v0/cat?arg={cid}", api.trim_end_matches('/'));
    let mut bytes = Vec::new();
    ureq::post(&url)
        .call()
        .with_context(|| format!("reading {cid} from IPFS at {api}"))?
        .into_reader()
        .take(1 << 20)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_are_abi_encoded() {
        assert_eq!(
            hex::encode(&Keccak256::digest("isAuthorized(address,address,address)")[..4]),
            "7c145cc7"
        );
        let word = word("0x00669A4CF01450B64E8A2A20E9b1FCB71E61eF03").unwrap();
        assert_eq!(&word[..12], &[0; 12]);
        assert_eq!(hex::encode(&word[12..]), STAKING.trim_start_matches("0x"));
        assert!(super::word("0x1234").is_err());
    }

    /// Against Arbitrum One: `cargo test -- --ignored`. An indexer and the
    /// operator it authorised on 2026-09 (an `OperatorSet` event), and a
    /// stranger.
    #[test]
    #[ignore]
    fn live_stake_and_operator_on_arbitrum_one() {
        const INDEXER: &str = "0xfeff9093f6b32d0e5cddba743b06a1fedb87c004";
        const OPERATOR: &str = "0x15e9fbe0b4b4daff67414b4facd362c584d1fae2";
        const STRANGER: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
        let staking = RpcStaking::new("https://arb1.arbitrum.io/rpc");
        assert!(staking.tokens_available(INDEXER).unwrap() > 0);
        assert_eq!(staking.tokens_available(STRANGER).unwrap(), 0);
        assert!(staking.is_authorized(INDEXER, OPERATOR).unwrap());
        assert!(!staking.is_authorized(INDEXER, STRANGER).unwrap());
    }

    #[test]
    fn cids_are_told_from_paths() {
        assert!(is_cid("QmYuYmYGouAkVZ6AnzcP1WPRpjFa2eh11xMTHGrTepvK2f"));
        assert!(!is_cid("work/attestation.json"));
    }
}
