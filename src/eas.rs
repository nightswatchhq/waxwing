//! Attestations on the Ethereum Attestation Service on Arbitrum One, where
//! they can be found by deployment rather than passed around by CID. The
//! attester is whoever sent the transaction, so the chain vouches for the
//! signer; the recipient is the indexer it speaks for.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use k256::ecdsa::SigningKey;
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};

use super::BlockPtr;
use super::attest::{Attestation, Claim};

pub const EAS: &str = "0xbd75f629a22dc1ced33dda0b68c546a1c035c458";
pub const SCHEMA_REGISTRY: &str = "0xa310da9c5b885e7fb3fba9d66e9ba6df512b78eb";
pub const SCHEMA: &str =
    "string deployment,uint32 block,bytes32 blockHash,uint32 from,bytes32 stateRoot";
/// No waxwing attestation can be older than this Arbitrum One block, which
/// the code postdates.
pub const SINCE: u64 = 512_000_000;
/// Blocks per `eth_getLogs` request.
const LOG_RANGE: u64 = 1_000_000;

/// The schema's UID, as the registry derives it: no resolver, revocable.
pub fn schema_uid() -> [u8; 32] {
    let mut packed = SCHEMA.as_bytes().to_vec();
    packed.extend_from_slice(&[0; 20]);
    packed.push(1);
    Keccak256::digest(packed).into()
}

fn uint(value: u64) -> [u8; 32] {
    let mut word = [0; 32];
    word[24..].copy_from_slice(&value.to_be_bytes());
    word
}

fn bytes32(hex_value: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(hex_value.trim_start_matches("0x"))
        .with_context(|| format!("{hex_value} is not hex"))?;
    bytes
        .try_into()
        .map_err(|_| anyhow!("{hex_value} is not 32 bytes"))
}

fn address_word(address: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(address.trim_start_matches("0x"))
        .with_context(|| format!("{address} is not an address"))?;
    if bytes.len() != 20 {
        bail!("{address} is not an address");
    }
    let mut word = [0; 32];
    word[12..].copy_from_slice(&bytes);
    Ok(word)
}

fn padded(bytes: &[u8]) -> Vec<u8> {
    let mut out = uint(bytes.len() as u64).to_vec();
    out.extend_from_slice(bytes);
    out.resize(32 + bytes.len().div_ceil(32) * 32, 0);
    out
}

fn word_at(data: &[u8], offset: usize) -> Result<&[u8]> {
    data.get(offset..offset + 32)
        .context("ABI data is shorter than its layout")
}

fn uint_at(data: &[u8], offset: usize) -> Result<u64> {
    let word = word_at(data, offset)?;
    if word[..24].iter().any(|b| *b != 0) {
        bail!("ABI integer does not fit in 64 bits");
    }
    Ok(u64::from_be_bytes(word[24..].try_into().unwrap()))
}

/// The attestation's fields, ABI-encoded as the schema lays them out.
fn encode_data(attestation: &Attestation) -> Result<Vec<u8>> {
    let mut out = uint(5 * 32).to_vec();
    out.extend_from_slice(&uint(u64::try_from(attestation.block.number)?));
    out.extend_from_slice(&bytes32(&attestation.block.hash)?);
    out.extend_from_slice(&uint(u64::try_from(attestation.from)?));
    out.extend_from_slice(&bytes32(&attestation.state_root)?);
    out.extend_from_slice(&padded(attestation.deployment.as_bytes()));
    Ok(out)
}

fn decode_data(data: &[u8]) -> Result<(String, BlockPtr, i32, String)> {
    let at = usize::try_from(uint_at(data, 0)?)?;
    let len = usize::try_from(uint_at(data, at)?)?;
    let deployment = data
        .get(at + 32..at + 32 + len)
        .context("deployment runs past the data")?;
    let block = BlockPtr {
        number: i32::try_from(uint_at(data, 32)?)?,
        hash: format!("0x{}", hex::encode(word_at(data, 64)?)),
    };
    let from = i32::try_from(uint_at(data, 96)?)?;
    let root = format!("0x{}", hex::encode(word_at(data, 128)?));
    Ok((String::from_utf8(deployment.to_vec())?, block, from, root))
}

/// `attest((schema, (recipient, expirationTime, revocable, refUID, data, value)))`
fn attest_call(recipient: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = hex::decode("f17325e7").unwrap();
    out.extend_from_slice(&uint(0x20));
    out.extend_from_slice(&schema_uid());
    out.extend_from_slice(&uint(0x40));
    out.extend_from_slice(&address_word(recipient)?);
    out.extend_from_slice(&uint(0));
    out.extend_from_slice(&uint(1));
    out.extend_from_slice(&[0; 32]);
    out.extend_from_slice(&uint(6 * 32));
    out.extend_from_slice(&uint(0));
    out.extend_from_slice(&padded(data));
    Ok(out)
}

fn rpc(url: &str, method: &str, params: Value) -> Result<Value> {
    let response: Value = ureq::post(url)
        .send_json(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .with_context(|| format!("{method} on {url}"))?
        .into_json()?;
    if let Some(error) = response.get("error") {
        bail!("{url} refused {method}: {error}");
    }
    Ok(response["result"].clone())
}

fn quantity(value: &Value) -> Result<u64> {
    let text = value.as_str().context("expected a hex quantity")?;
    u64::from_str_radix(text.trim_start_matches("0x"), 16)
        .with_context(|| format!("{text} is not a quantity"))
}

/// Minimal big-endian bytes, as RLP wants integers.
fn minimal(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len());
    &bytes[start..]
}

fn rlp_length(len: usize, short: u8, out: &mut Vec<u8>) {
    if len <= 55 {
        out.push(short + len as u8);
    } else {
        let len_bytes = len.to_be_bytes();
        let len_bytes = minimal(&len_bytes);
        out.push(short + 55 + len_bytes.len() as u8);
        out.extend_from_slice(len_bytes);
    }
}

fn rlp_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    if bytes.len() == 1 && bytes[0] < 0x80 {
        out.push(bytes[0]);
    } else {
        rlp_length(bytes.len(), 0x80, out);
        out.extend_from_slice(bytes);
    }
}

fn rlp_list(items: &[&[u8]]) -> Vec<u8> {
    let mut payload = Vec::new();
    for item in items {
        rlp_bytes(item, &mut payload);
    }
    let mut out = Vec::new();
    rlp_length(payload.len(), 0xc0, &mut out);
    out.extend_from_slice(&payload);
    out
}

fn sender(key: &SigningKey) -> String {
    let point = key.verifying_key().to_sec1_point(false);
    format!(
        "0x{}",
        hex::encode(&Keccak256::digest(&point.as_bytes()[1..])[12..])
    )
}

/// Send `data` to `to` as a legacy EIP-155 transaction and wait for its
/// receipt.
fn send(url: &str, key: &SigningKey, to: &str, data: &[u8]) -> Result<Value> {
    let from = sender(key);
    let call = json!({ "from": from, "to": to, "data": format!("0x{}", hex::encode(data)) });
    let chain = quantity(&rpc(url, "eth_chainId", json!([]))?)?;
    let nonce = quantity(&rpc(
        url,
        "eth_getTransactionCount",
        json!([from, "pending"]),
    )?)?;
    let gas_price = quantity(&rpc(url, "eth_gasPrice", json!([]))?)? * 2;
    let gas = quantity(&rpc(url, "eth_estimateGas", json!([call]))?)? * 3 / 2;

    let to = hex::decode(to.trim_start_matches("0x"))?;
    let fields = |extra: [&[u8]; 3]| -> Vec<u8> {
        let (nonce, gas_price, gas) = (
            nonce.to_be_bytes(),
            gas_price.to_be_bytes(),
            gas.to_be_bytes(),
        );
        rlp_list(&[
            minimal(&nonce),
            minimal(&gas_price),
            minimal(&gas),
            &to,
            &[],
            data,
            extra[0],
            extra[1],
            extra[2],
        ])
    };
    let chain_bytes = chain.to_be_bytes();
    let unsigned = fields([minimal(&chain_bytes), &[], &[]]);
    let (signature, recovery) = key.sign_prehash_recoverable(&Keccak256::digest(&unsigned));
    let v = (u64::from(recovery.to_byte()) + 35 + 2 * chain).to_be_bytes();
    let rs = signature.to_bytes();
    let raw = fields([minimal(&v), minimal(&rs[..32]), minimal(&rs[32..])]);

    let hash = rpc(
        url,
        "eth_sendRawTransaction",
        json!([format!("0x{}", hex::encode(raw))]),
    )?;
    let started = Instant::now();
    loop {
        let receipt = rpc(url, "eth_getTransactionReceipt", json!([hash]))?;
        if !receipt.is_null() {
            if quantity(&receipt["status"])? != 1 {
                bail!("transaction {hash} reverted");
            }
            return Ok(receipt);
        }
        if started.elapsed() > Duration::from_secs(120) {
            bail!("transaction {hash} was not mined within two minutes");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn attested_topic() -> String {
    format!(
        "0x{}",
        hex::encode(Keccak256::digest(
            "Attested(address,address,bytes32,bytes32)"
        ))
    )
}

/// Put `attestation` on EAS, sent with `key`, and return its UID. The
/// attestation's own signature is not needed: the transaction is one.
pub fn publish(url: &str, key: &str, attestation: &Attestation) -> Result<String> {
    let key = hex::decode(key.trim().trim_start_matches("0x")).context("key is not hex")?;
    let key = SigningKey::from_slice(&key).map_err(|_| anyhow!("not a secp256k1 private key"))?;
    let indexer = attestation.indexer.clone().unwrap_or_else(|| sender(&key));
    let call = attest_call(&indexer, &encode_data(attestation)?)?;
    let receipt = send(url, &key, EAS, &call)?;
    let logs = receipt["logs"].as_array().context("receipt has no logs")?;
    let topic = attested_topic();
    logs.iter()
        .find(|log| log["topics"][0].as_str() == Some(topic.as_str()))
        .and_then(|log| log["data"].as_str())
        .map(str::to_string)
        .context("the transaction emitted no Attested event")
}

/// Every unrevoked attestation of `deployment` under waxwing's schema since
/// block `since`, with the chain's word for who sent it.
pub fn find(url: &str, deployment: &str, since: u64) -> Result<Vec<Claim>> {
    let head = quantity(&rpc(url, "eth_blockNumber", json!([]))?)?;
    let schema = format!("0x{}", hex::encode(schema_uid()));
    let mut claims = Vec::new();
    let mut from = since;
    while from <= head {
        let to = (from + LOG_RANGE - 1).min(head);
        let logs = rpc(
            url,
            "eth_getLogs",
            json!([{
                "address": EAS,
                "topics": [attested_topic(), null, null, schema],
                "fromBlock": format!("0x{from:x}"),
                "toBlock": format!("0x{to:x}"),
            }]),
        )?;
        for log in logs.as_array().context("eth_getLogs returned no list")? {
            let uid = log["data"].as_str().context("Attested without a UID")?;
            let mut call = Keccak256::digest("getAttestation(bytes32)")[..4].to_vec();
            call.extend_from_slice(&bytes32(uid)?);
            let result = rpc(
                url,
                "eth_call",
                json!([{ "to": EAS, "data": format!("0x{}", hex::encode(call)) }, "latest"]),
            )?;
            let result = hex::decode(
                result
                    .as_str()
                    .context("no result")?
                    .trim_start_matches("0x"),
            )?;
            // (uid, schema, time, expirationTime, revocationTime, refUID,
            //  recipient, attester, revocable, data)
            let tuple = usize::try_from(uint_at(&result, 0)?)?;
            let field = |i: usize| word_at(&result, tuple + 32 * i);
            if uint_at(&result, tuple + 32 * 4)? != 0 {
                continue;
            }
            let recipient = format!("0x{}", hex::encode(&field(6)?[12..]));
            let attester = format!("0x{}", hex::encode(&field(7)?[12..]));
            let data_at = tuple + usize::try_from(uint_at(&result, tuple + 32 * 9)?)?;
            let len = usize::try_from(uint_at(&result, data_at)?)?;
            let data = result
                .get(data_at + 32..data_at + 32 + len)
                .context("attestation data runs past the result")?;
            let Ok((found, block, from_block, state_root)) = decode_data(data) else {
                continue;
            };
            if found != deployment {
                continue;
            }
            claims.push(Claim {
                attestation: Attestation {
                    version: 2,
                    deployment: found,
                    block,
                    from: from_block,
                    state_root,
                    indexer: Some(recipient),
                    signer: attester.clone(),
                    signature: String::new(),
                },
                signer: attester,
            });
        }
        from = to + 1;
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attestation() -> Attestation {
        Attestation {
            version: 2,
            deployment: "QmYuYmYGouAkVZ6AnzcP1WPRpjFa2eh11xMTHGrTepvK2f".into(),
            block: BlockPtr {
                number: 512191262,
                hash: format!("0x{}", "4f".repeat(32)),
            },
            from: 59,
            state_root: format!("0x{}", "ab".repeat(32)),
            indexer: Some("0xfeff9093f6b32d0e5cddba743b06a1fedb87c004".into()),
            signer: String::new(),
            signature: String::new(),
        }
    }

    #[test]
    fn data_round_trips() {
        let a = attestation();
        let (deployment, block, from, root) = decode_data(&encode_data(&a).unwrap()).unwrap();
        assert_eq!(
            (deployment, block, from, root),
            (a.deployment, a.block, a.from, a.state_root)
        );
    }

    #[test]
    fn rlp_matches_the_spec_examples() {
        let mut out = Vec::new();
        rlp_bytes(b"dog", &mut out);
        assert_eq!(out, [0x83, b'd', b'o', b'g']);
        assert_eq!(
            rlp_list(&[b"cat", b"dog"]),
            [0xc8, 0x83, b'c', b'a', b't', 0x83, b'd', b'o', b'g']
        );
        assert_eq!(rlp_list(&[]), [0xc0]);
        let mut out = Vec::new();
        rlp_bytes(&[], &mut out);
        assert_eq!(out, [0x80]);
        let long = [b'a'; 56];
        let mut out = Vec::new();
        rlp_bytes(&long, &mut out);
        assert_eq!(&out[..2], &[0xb8, 56]);
    }

    #[test]
    fn the_attest_call_is_laid_out_as_eas_expects() {
        let call = attest_call("0xfeff9093f6b32d0e5cddba743b06a1fedb87c004", b"hi").unwrap();
        assert_eq!(hex::encode(&call[..4]), "f17325e7");
        // selector; offset, schema, offset; six inner words; data length, data
        assert_eq!(call.len(), 4 + 32 * 11);
        assert_eq!(uint_at(&call[4..], 32 * 7).unwrap(), 6 * 32);
        assert_eq!(uint_at(&call[4..], 32 * 9).unwrap(), 2);
    }
}
