use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use k256::ecdsa::{RecoveryId, Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use super::BlockPtr;
use super::network::Staking;

pub const ATTESTATION_VERSION: u32 = 2;

/// An indexer's signed claim that, having indexed a deployment itself, its
/// entity versions as of `block` hash to `state_root`.
///
/// Signed as an Ethereum personal message, so the signer is an address and
/// any wallet or operator key can produce one. From version 2 it names the
/// indexer it speaks for, which is the signer unless an operator signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub version: u32,
    pub deployment: String,
    pub block: BlockPtr,
    pub state_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<String>,
    pub signer: String,
    pub signature: String,
}

fn hex0x(value: &str) -> String {
    format!("0x{}", value.trim_start_matches("0x").to_ascii_lowercase())
}

/// The text that is signed. Hashes are normalised so that two indexers who
/// read the same block hash in different spellings sign the same bytes.
pub fn statement(
    version: u32,
    deployment: &str,
    block: &BlockPtr,
    state_root: &str,
    indexer: Option<&str>,
) -> String {
    let mut out = format!(
        "waxwing attestation v{version}\ndeployment: {deployment}\nblock: {} {}\nstate: {}",
        block.number,
        hex0x(&block.hash),
        hex0x(state_root)
    );
    if let Some(indexer) = indexer {
        out.push_str(&format!("\nindexer: {}", hex0x(indexer)));
    }
    out
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

/// Sign with a hex secp256k1 private key, for `indexer`, or for the key's
/// own address.
pub fn attest(
    key: &str,
    deployment: &str,
    block: &BlockPtr,
    state_root: &str,
    indexer: Option<&str>,
) -> Result<Attestation> {
    let key = hex::decode(key.trim().trim_start_matches("0x")).context("key is not hex")?;
    let key = SigningKey::from_slice(&key).map_err(|_| anyhow!("not a secp256k1 private key"))?;
    let signer = address(key.verifying_key());
    let indexer = hex0x(indexer.unwrap_or(&signer));
    let statement = statement(
        ATTESTATION_VERSION,
        deployment,
        block,
        state_root,
        Some(&indexer),
    );
    let (signature, recovery) = key.sign_prehash_recoverable(&digest(&statement));
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
        indexer: Some(indexer),
        signer,
        signature: format!("0x{}", hex::encode(bytes)),
    })
}

impl Attestation {
    fn statement(&self) -> Result<String> {
        let indexer = match (self.version, &self.indexer) {
            (1, None) => None,
            (2, Some(indexer)) => Some(indexer.as_str()),
            (1 | 2, _) => bail!("version {} with indexer {:?}", self.version, self.indexer),
            (version, _) => bail!("unknown attestation version {version}"),
        };
        Ok(statement(
            self.version,
            &self.deployment,
            &self.block,
            &self.state_root,
            indexer,
        ))
    }

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
        let digest = digest(&self.statement()?);
        let key = VerifyingKey::recover_from_prehash(&digest, &signature, recovery)
            .map_err(|_| anyhow!("signature does not recover to a key"))?;
        Ok(address(&key))
    }

    /// Whether this makes the same claim as `other`, whoever signed it.
    pub fn agrees_with(&self, other: &Attestation) -> bool {
        self.deployment == other.deployment
            && self.block.number == other.block.number
            && hex0x(&self.block.hash) == hex0x(&other.block.hash)
            && hex0x(&self.state_root) == hex0x(&other.state_root)
    }
}

/// Who an agreeing attestation counts for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agreement {
    pub indexer: String,
    pub signer: String,
    /// Tokens the indexer has behind the subgraph service, when checked.
    pub stake: Option<u128>,
}

/// The distinct indexers whose validly signed attestations say what
/// `expected` says, among `allowed` if any are given.
///
/// With `staking`, an indexer counts only with stake provisioned to the
/// subgraph service, and only if it signed itself or authorised the
/// operator who did. Without it, nothing ties a key to an indexer, so each
/// counts as its own signer, and keys are free: the tally then says only
/// that somebody agrees.
pub fn tally(
    expected: &Attestation,
    attestations: &[Attestation],
    allowed: &[String],
    staking: Option<&dyn Staking>,
) -> Result<Vec<Agreement>> {
    let allowed: Vec<String> = allowed.iter().map(|a| hex0x(a)).collect();
    let mut counted: BTreeMap<String, Agreement> = BTreeMap::new();
    for attestation in attestations.iter().filter(|a| a.agrees_with(expected)) {
        let Ok(signer) = attestation.recover() else {
            continue;
        };
        let claimed = attestation.indexer.as_deref().map(hex0x);
        let (indexer, stake) = match staking {
            None => (signer.clone(), None),
            Some(staking) => {
                let indexer = claimed.unwrap_or_else(|| signer.clone());
                let stake = staking.tokens_available(&indexer)?;
                if stake == 0 {
                    continue;
                }
                if indexer != signer && !staking.is_authorized(&indexer, &signer)? {
                    continue;
                }
                (indexer, Some(stake))
            }
        };
        if !allowed.is_empty() && !allowed.contains(&indexer) {
            continue;
        }
        counted.entry(indexer.clone()).or_insert(Agreement {
            indexer,
            signer,
            stake,
        });
    }
    Ok(counted.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    // anvil's first three accounts
    const KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const ADDRESS_0: &str = "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266";
    const KEY_1: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const ADDRESS_1: &str = "0x70997970c51812dc3a010c7d01b50e0d17dc79c8";
    const KEY_2: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    const ADDRESS_2: &str = "0x3c44cdddb6a900fa2b585dd299e03d12fa4293bc";

    fn block() -> BlockPtr {
        BlockPtr {
            number: 40,
            hash: "20CA45".into(),
        }
    }

    fn signed(key: &str, root: &str) -> Attestation {
        attest(key, "QmTest", &block(), root, None).unwrap()
    }

    fn signers(agreements: &[Agreement]) -> Vec<&str> {
        agreements.iter().map(|a| a.signer.as_str()).collect()
    }

    /// Indexer stakes, and the operators each has authorised.
    struct FakeStaking(
        HashMap<&'static str, u128>,
        HashSet<(&'static str, &'static str)>,
    );

    impl Staking for FakeStaking {
        fn tokens_available(&self, indexer: &str) -> Result<u128> {
            Ok(self.0.get(indexer).copied().unwrap_or(0))
        }
        fn is_authorized(&self, indexer: &str, operator: &str) -> Result<bool> {
            Ok(self.1.iter().any(|(i, o)| *i == indexer && *o == operator))
        }
    }

    #[test]
    fn the_signer_is_the_keys_ethereum_address() {
        let attestation = signed(KEY_0, "ab");
        assert_eq!(attestation.signer, ADDRESS_0);
        assert_eq!(attestation.indexer.as_deref(), Some(ADDRESS_0));
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
            statement(2, "QmTest", &block(), "0xAB", Some(ADDRESS_0)),
            statement(
                2,
                "QmTest",
                &other,
                "ab",
                Some(&ADDRESS_0.to_uppercase().replace("0X", "0x"))
            )
        );
    }

    #[test]
    fn a_version_1_attestation_still_verifies() {
        // As version 1 signed it: no indexer line.
        let key = SigningKey::from_slice(&hex::decode(KEY_1).unwrap()).unwrap();
        let text = statement(1, "QmTest", &block(), "ab", None);
        let (signature, recovery) = key.sign_prehash_recoverable(&digest(&text));
        let mut bytes = signature.to_bytes().to_vec();
        bytes.push(27 + recovery.to_byte());
        let old = Attestation {
            version: 1,
            deployment: "QmTest".into(),
            block: block(),
            state_root: "0xab".into(),
            indexer: None,
            signer: ADDRESS_1.into(),
            signature: format!("0x{}", hex::encode(bytes)),
        };
        assert_eq!(old.recover().unwrap(), ADDRESS_1);
        let expected = signed(KEY_0, "ab");
        assert_eq!(
            signers(&tally(&expected, &[old], &[], None).unwrap()),
            [ADDRESS_1]
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
        assert_eq!(
            signers(&tally(&expected, &all, &[], None).unwrap()),
            [ADDRESS_1, ADDRESS_0]
        );

        let allowed = [ADDRESS_1.to_uppercase().replace("0X", "0x")];
        assert_eq!(tally(&expected, &all, &allowed, None).unwrap().len(), 1);
    }

    #[test]
    fn tally_ignores_another_state_and_a_forged_signer() {
        let expected = signed(KEY_0, "ab");

        let disagrees = signed(KEY_1, "cd");
        assert!(
            tally(&expected, &[disagrees], &[], None)
                .unwrap()
                .is_empty()
        );

        // Key 1's signature over another root, relabelled as agreeing.
        let mut forged = signed(KEY_1, "cd");
        forged.state_root = "0xab".into();
        // It recovers to some key, just not key 1.
        assert!(
            tally(&expected, &[forged], &[ADDRESS_1.into()], None)
                .unwrap()
                .is_empty()
        );

        // Claiming to be someone else changes nothing: the key decides.
        let mut renamed = signed(KEY_1, "ab");
        renamed.signer = ADDRESS_0.into();
        assert_eq!(
            signers(&tally(&expected, &[renamed], &[], None).unwrap()),
            [ADDRESS_1]
        );
    }

    #[test]
    fn on_the_network_only_staked_indexers_and_their_operators_count() {
        let expected = signed(KEY_0, "ab");
        // 0 is an indexer; 1 is its operator; 2 has no stake and no say.
        let staking = FakeStaking(
            HashMap::from([(ADDRESS_0, 100)]),
            HashSet::from([(ADDRESS_0, ADDRESS_1)]),
        );
        let by_operator = attest(KEY_1, "QmTest", &block(), "ab", Some(ADDRESS_0)).unwrap();
        let by_stranger = attest(KEY_2, "QmTest", &block(), "ab", Some(ADDRESS_0)).unwrap();
        let unstaked = signed(KEY_2, "ab");

        let counted = tally(
            &expected,
            std::slice::from_ref(&by_operator),
            &[],
            Some(&staking),
        )
        .unwrap();
        assert_eq!(
            counted,
            [Agreement {
                indexer: ADDRESS_0.into(),
                signer: ADDRESS_1.into(),
                stake: Some(100)
            }]
        );
        assert!(
            tally(&expected, &[by_stranger, unstaked], &[], Some(&staking))
                .unwrap()
                .is_empty()
        );

        // The indexer and its operator are one indexer, counted once.
        let both = [signed(KEY_0, "ab"), by_operator.clone()];
        assert_eq!(
            tally(&expected, &both, &[], Some(&staking)).unwrap().len(),
            1
        );

        // The indexer is part of what was signed: relabelling it breaks the signature.
        let mut relabelled = by_operator;
        relabelled.indexer = Some(ADDRESS_2.into());
        assert_ne!(relabelled.recover().unwrap(), ADDRESS_1);
    }
}
