use std::collections::BTreeSet;

use anyhow::{Context, Result, anyhow, bail};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use super::BlockPtr;

pub const ATTESTATION_VERSION: u32 = 1;

/// An indexer's signed claim that, having indexed a deployment itself, its
/// entity versions as of `block` hash to `state_root`.
///
/// Signed as an Ethereum personal message, so the signer is an address and
/// any wallet or operator key can produce one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub version: u32,
    pub deployment: String,
    pub block: BlockPtr,
    pub state_root: String,
    pub signer: String,
    pub signature: String,
}

fn hex0x(value: &str) -> String {
    format!("0x{}", value.trim_start_matches("0x").to_ascii_lowercase())
}

/// The text that is signed. Hashes are normalised so that two indexers who
/// read the same block hash in different spellings sign the same bytes.
pub fn statement(deployment: &str, block: &BlockPtr, state_root: &str) -> String {
    format!(
        "waxwing attestation v{ATTESTATION_VERSION}\ndeployment: {deployment}\nblock: {} {}\nstate: {}",
        block.number,
        hex0x(&block.hash),
        hex0x(state_root)
    )
}

fn digest(statement: &str) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(format!("\x19Ethereum Signed Message:\n{}", statement.len()));
    hasher.update(statement);
    hasher.finalize().into()
}

fn address(key: &VerifyingKey) -> String {
    let point = key.to_sec1_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

/// Sign with a hex secp256k1 private key.
pub fn attest(
    key: &str,
    deployment: &str,
    block: &BlockPtr,
    state_root: &str,
) -> Result<Attestation> {
    let key = hex::decode(key.trim().trim_start_matches("0x")).context("key is not hex")?;
    let key = SigningKey::from_slice(&key).map_err(|_| anyhow!("not a secp256k1 private key"))?;
    let (signature, recovery) =
        key.sign_prehash_recoverable(&digest(&statement(deployment, block, state_root)));
    let mut bytes = signature.to_bytes().to_vec();
    bytes.push(27 + recovery.to_byte());
    Ok(Attestation {
        version: ATTESTATION_VERSION,
        deployment: deployment.to_string(),
        block: BlockPtr {
            number: block.number,
            hash: hex0x(&block.hash),
        },
        state_root: hex0x(state_root),
        signer: address(key.verifying_key()),
        signature: format!("0x{}", hex::encode(bytes)),
    })
}

impl Attestation {
    /// The address whose key signed this, whatever `signer` claims.
    pub fn recover(&self) -> Result<String> {
        let bytes =
            hex::decode(self.signature.trim_start_matches("0x")).context("signature is not hex")?;
        let [signature @ .., v] = bytes.as_slice() else {
            bail!("empty signature");
        };
        let signature = Signature::from_slice(signature).map_err(|_| anyhow!("bad signature"))?;
        let recovery = RecoveryId::from_byte(v.wrapping_sub(27))
            .or_else(|| RecoveryId::from_byte(*v))
            .context("bad recovery byte")?;
        let digest = digest(&statement(&self.deployment, &self.block, &self.state_root));
        let key = VerifyingKey::recover_from_prehash(&digest, &signature, recovery)
            .map_err(|_| anyhow!("signature does not recover to a key"))?;
        Ok(address(&key))
    }
}

/// The distinct signers, among `allowed` if any are given, whose
/// attestations are validly signed and say what `expected` says. `signer`
/// and `signature` of `expected` are ignored.
///
/// With no `allowed` list every key counts, and keys are free: the tally
/// then says only that somebody agrees. Whether a signer is an indexer
/// with stake is for the caller to know.
pub fn tally(
    expected: &Attestation,
    attestations: &[Attestation],
    allowed: &[String],
) -> BTreeSet<String> {
    let allowed: BTreeSet<String> = allowed.iter().map(|a| hex0x(a)).collect();
    let claim = |a: &Attestation| statement(&a.deployment, &a.block, &a.state_root);
    attestations
        .iter()
        .filter(|a| a.version == ATTESTATION_VERSION && claim(a) == claim(expected))
        .filter_map(|a| a.recover().ok())
        .filter(|signer| allowed.is_empty() || allowed.contains(signer))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // anvil's first two accounts
    const KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const ADDRESS_0: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    const KEY_1: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const ADDRESS_1: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";

    fn block() -> BlockPtr {
        BlockPtr {
            number: 40,
            hash: "20CA45".into(),
        }
    }

    fn signed(key: &str, root: &str) -> Attestation {
        attest(key, "QmTest", &block(), root).unwrap()
    }

    #[test]
    fn the_signer_is_the_keys_ethereum_address() {
        let attestation = signed(KEY_0, "ab");
        assert_eq!(attestation.signer, ADDRESS_0);
        assert_eq!(attestation.recover().unwrap(), ADDRESS_0);
        assert_eq!(signed(KEY_1, "ab").signer, ADDRESS_1);
        assert_eq!(attestation.signature.len(), 2 + 65 * 2);
    }

    #[test]
    fn hash_spelling_does_not_change_what_is_signed() {
        let other = BlockPtr {
            number: 40,
            hash: "0x20ca45".into(),
        };
        assert_eq!(
            statement("QmTest", &block(), "0xAB"),
            statement("QmTest", &other, "ab")
        );
    }

    #[test]
    fn tally_counts_distinct_valid_signers_who_agree() {
        let expected = signed(KEY_0, "ab");
        let all = [
            signed(KEY_0, "ab"),
            signed(KEY_0, "ab"),
            signed(KEY_1, "ab"),
        ];
        let both: BTreeSet<String> = [ADDRESS_0, ADDRESS_1].map(String::from).into();
        assert_eq!(tally(&expected, &all, &[]), both);

        let allowed = [ADDRESS_1.to_uppercase().replace("0X", "0x")];
        assert_eq!(tally(&expected, &all, &allowed).len(), 1);
    }

    #[test]
    fn tally_ignores_another_state_and_a_forged_signer() {
        let expected = signed(KEY_0, "ab");

        let disagrees = signed(KEY_1, "cd");
        assert!(tally(&expected, &[disagrees], &[]).is_empty());

        // Key 1's signature over another root, relabelled as agreeing.
        let mut forged = signed(KEY_1, "cd");
        forged.state_root = "0xab".into();
        let signers = tally(&expected, &[forged], &[ADDRESS_1.into()]);
        assert!(signers.is_empty(), "{signers:?}");

        // Claiming to be someone else changes nothing: the key decides.
        let mut renamed = signed(KEY_1, "ab");
        renamed.signer = ADDRESS_0.into();
        assert_eq!(
            tally(&expected, &[renamed], &[])
                .into_iter()
                .next()
                .unwrap(),
            ADDRESS_1
        );
    }
}
