use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

mod diff;
pub use diff::{Diff, State, TableDiff, TableState, diff, state};

pub const CATALOGUE_FILE: &str = "catalogue.json";
pub const CATALOGUE_VERSION: u32 = 1;

const METADATA_FILE: &str = "metadata.json";
const SCHEMA_FILE: &str = "schema.graphql";
const MANIFEST_FILE: &str = "subgraph.yaml";

/// The subset of graph-node's dump `metadata.json` (format version 1) that
/// the catalogue binds to. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct DumpMetadata {
    version: u32,
    deployment: String,
    network: String,
    manifest: DumpManifest,
    earliest_block_number: i32,
    head_block: Option<BlockPtr>,
    graft_base: Option<String>,
    graft_block: Option<BlockPtr>,
    tables: BTreeMap<String, DumpTable>,
}

#[derive(Debug, Deserialize)]
struct DumpManifest {
    history_blocks: i32,
}

#[derive(Debug, Deserialize)]
struct DumpTable {
    chunks: Vec<DumpChunk>,
    #[serde(default)]
    clamps: Vec<DumpChunk>,
}

#[derive(Debug, Deserialize)]
struct DumpChunk {
    file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockPtr {
    pub number: i32,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// One `graphman dump` run: the head it was taken at and the data files it
/// added. Rows in those files are only as canonical as that head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub head_block: BlockPtr,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalogue {
    pub version: u32,
    pub deployment: String,
    pub network: String,
    pub head_block: BlockPtr,
    pub earliest_block_number: i32,
    pub history_blocks: i32,
    pub graft_base: Option<String>,
    pub graft_block: Option<BlockPtr>,
    pub graph_node_version: Option<String>,
    pub public_poi: Option<String>,
    /// Oldest first; the last layer's head is `head_block`.
    pub layers: Vec<Layer>,
    /// Sorted by path.
    pub files: Vec<FileEntry>,
    /// SHA-256 over the `files` list; identifies this artefact, not the
    /// deployment's state. Parquet bytes differ between publishers.
    pub root: String,
}

/// The receiver's or publisher's own view of the chain.
pub trait Chain {
    /// Hash of the canonical block at `number`, if the chain has one.
    fn block_hash(&self, number: i32) -> Result<Option<String>>;
}

pub struct RpcChain {
    url: String,
}

impl RpcChain {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }
}

impl Chain for RpcChain {
    fn block_hash(&self, number: i32) -> Result<Option<String>> {
        let response: serde_json::Value = ureq::post(&self.url)
            .send_json(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_getBlockByNumber",
                "params": [format!("0x{number:x}"), false],
            }))
            .with_context(|| format!("asking {} for block {number}", self.url))?
            .into_json()?;
        if let Some(error) = response.get("error") {
            bail!("{} refused block {number}: {error}", self.url);
        }
        Ok(response["result"]["hash"].as_str().map(str::to_string))
    }
}

fn same_hash(a: &str, b: &str) -> bool {
    let strip = |h: &str| h.trim_start_matches("0x").to_ascii_lowercase();
    strip(a) == strip(b)
}

/// Layers whose head is no longer on the chain. Everything such a layer
/// added was indexed on a fork that has since been reverted.
fn reverted_layers(layers: &[Layer], chain: &dyn Chain) -> Result<Vec<BlockPtr>> {
    let mut reverted = Vec::new();
    for layer in layers {
        let head = &layer.head_block;
        let canonical = chain.block_hash(head.number)?;
        if !canonical.is_some_and(|hash| same_hash(&hash, &head.hash)) {
            reverted.push(head.clone());
        }
    }
    Ok(reverted)
}

#[derive(Default)]
pub struct SealOptions<'a> {
    pub graph_node_version: Option<String>,
    pub public_poi: Option<String>,
    /// When given, every layer's head must still be on this chain.
    pub chain: Option<&'a dyn Chain>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    Missing(String),
    Size {
        path: String,
        expected: u64,
        actual: u64,
    },
    Hash(String),
    Root,
    /// Referenced by `metadata.json` but absent from the catalogue.
    Unlisted(String),
    /// A layer was dumped at a block that is no longer on the chain.
    Reverted(BlockPtr),
    /// The layers do not end at the catalogue's head block.
    Layers,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Problem::Missing(p) => write!(f, "{p}: missing"),
            Problem::Size {
                path,
                expected,
                actual,
            } => write!(f, "{path}: {actual} bytes, catalogue says {expected}"),
            Problem::Hash(p) => write!(f, "{p}: sha256 mismatch"),
            Problem::Root => write!(f, "root does not match the file list"),
            Problem::Unlisted(p) => write!(f, "{p}: in metadata.json but not in the catalogue"),
            Problem::Reverted(head) => write!(
                f,
                "a layer was dumped at block {} ({}), which is not on the chain: it holds reverted rows",
                head.number, head.hash
            ),
            Problem::Layers => write!(f, "layers do not end at the catalogue's head block"),
        }
    }
}

fn read_metadata(dir: &Path) -> Result<DumpMetadata> {
    let path = dir.join(METADATA_FILE);
    let raw = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let metadata: DumpMetadata =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if metadata.version != 1 {
        bail!("unsupported dump format version {}", metadata.version);
    }
    Ok(metadata)
}

/// Paths come from files a stranger may have written, so they must stay
/// inside the dump directory.
fn check_relative(path: &str) -> Result<()> {
    let ok = !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if !ok {
        bail!("path '{path}' escapes the dump directory");
    }
    Ok(())
}

fn dump_files(dir: &Path, metadata: &DumpMetadata) -> Result<Vec<String>> {
    let mut paths = vec![METADATA_FILE.to_string(), SCHEMA_FILE.to_string()];
    if dir.join(MANIFEST_FILE).exists() {
        paths.push(MANIFEST_FILE.to_string());
    }
    for table in metadata.tables.values() {
        for chunk in table.chunks.iter().chain(&table.clamps) {
            check_relative(&chunk.file)?;
            paths.push(chunk.file.clone());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut bytes = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((bytes, hex::encode(hasher.finalize())))
}

fn root(files: &[FileEntry]) -> String {
    let mut hasher = Sha256::new();
    for f in files {
        hasher.update(f.path.as_bytes());
        hasher.update([0]);
        hasher.update(f.bytes.to_be_bytes());
        hasher.update(f.sha256.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// The data files this dump run added, given what was already sealed.
///
/// graphman records only the latest head, so a run that was never sealed
/// leaves files whose head nobody knows. One new chunk and one new clamp
/// file per table is all a single run writes; more means a run was missed.
fn new_data_files(metadata: &DumpMetadata, prev: Option<&Catalogue>) -> Result<Vec<String>> {
    let sealed = |path: &str| prev.is_some_and(|p| p.files.iter().any(|f| f.path == path));
    let mut fresh = Vec::new();
    for (name, table) in &metadata.tables {
        let lists = [
            ("chunk", &table.chunks, 1),
            ("clamp", &table.clamps, usize::from(prev.is_some())),
        ];
        for (kind, list, allowed) in lists {
            let unsealed: Vec<_> = list.iter().filter(|c| !sealed(&c.file)).collect();
            if unsealed.len() > allowed {
                bail!(
                    "{name} has {} unsealed {kind} files: more than one dump has run since the \
                     last seal, so the blocks they were taken at are unknown. Dump afresh into \
                     an empty directory and seal after every dump",
                    unsealed.len()
                );
            }
            fresh.extend(unsealed.into_iter().map(|c| c.file.clone()));
        }
    }
    fresh.sort();
    Ok(fresh)
}

/// Hash every file a dump's `metadata.json` references and write
/// `catalogue.json` beside it. Run it after every `graphman dump` into the
/// directory, incremental ones included.
pub fn seal(dir: &Path, opts: SealOptions) -> Result<Catalogue> {
    let metadata = read_metadata(dir)?;
    let Some(head_block) = metadata.head_block.clone() else {
        bail!("dump has no head block, nothing to seal");
    };
    let prev = if dir.join(CATALOGUE_FILE).exists() {
        Some(read_catalogue(dir)?)
    } else {
        None
    };
    if let Some(prev) = &prev
        && prev.deployment != metadata.deployment
    {
        bail!(
            "catalogue is for {}, dump is of {}",
            prev.deployment,
            metadata.deployment
        );
    }

    let fresh = new_data_files(&metadata, prev.as_ref())?;

    let mut files = Vec::new();
    for path in dump_files(dir, &metadata)? {
        let (bytes, sha256) =
            hash_file(&dir.join(&path)).with_context(|| format!("hashing {path}"))?;
        files.push(FileEntry {
            path,
            bytes,
            sha256,
        });
    }

    let mut layers = Vec::new();
    if let Some(prev) = prev {
        for old in prev.files.iter().filter(|f| f.path.ends_with(".parquet")) {
            if !files.contains(old) {
                bail!("{} has changed since it was sealed", old.path);
            }
        }
        layers = prev.layers;
    }
    let last = layers.last().map(|l: &Layer| &l.head_block);
    if last != Some(&head_block) {
        if last.is_some_and(|last| last.number >= head_block.number) {
            bail!("dump head has not advanced past the last sealed layer");
        }
        layers.push(Layer {
            head_block: head_block.clone(),
            files: fresh,
        });
    } else if !fresh.is_empty() {
        bail!("new data files at an already sealed head");
    }

    if let Some(chain) = opts.chain
        && let Some(head) = reverted_layers(&layers, chain)?.first()
    {
        return Err(anyhow!(
            "a layer was dumped at block {} ({}), which is no longer on the chain. The \
             directory holds rows from a reverted fork and graphman will not remove them: \
             dump afresh into an empty directory",
            head.number,
            head.hash
        ));
    }

    let catalogue = Catalogue {
        version: CATALOGUE_VERSION,
        deployment: metadata.deployment,
        network: metadata.network,
        head_block,
        earliest_block_number: metadata.earliest_block_number,
        history_blocks: metadata.manifest.history_blocks,
        graft_base: metadata.graft_base,
        graft_block: metadata.graft_block,
        graph_node_version: opts.graph_node_version,
        public_poi: opts.public_poi,
        layers,
        root: root(&files),
        files,
    };

    let tmp = dir.join(format!("{CATALOGUE_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_vec_pretty(&catalogue)?)?;
    fs::rename(&tmp, dir.join(CATALOGUE_FILE))?;
    Ok(catalogue)
}

pub fn read_catalogue(dir: &Path) -> Result<Catalogue> {
    let path = dir.join(CATALOGUE_FILE);
    let raw = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let catalogue: Catalogue =
        serde_json::from_slice(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if catalogue.version != CATALOGUE_VERSION {
        bail!("unsupported catalogue version {}", catalogue.version);
    }
    Ok(catalogue)
}

/// Re-hash a sealed dump against its catalogue. An empty result means the
/// bytes are the ones the publisher sealed; it says nothing about whether
/// the publisher's data was right. With a chain, also checks that no layer
/// was dumped on a fork that chain has since reverted.
pub fn verify(dir: &Path, chain: Option<&dyn Chain>) -> Result<(Catalogue, Vec<Problem>)> {
    let catalogue = read_catalogue(dir)?;
    let mut problems = Vec::new();

    if catalogue.layers.last().map(|l| &l.head_block) != Some(&catalogue.head_block) {
        problems.push(Problem::Layers);
    }
    if let Some(chain) = chain {
        let reverted = reverted_layers(&catalogue.layers, chain)?;
        problems.extend(reverted.into_iter().map(Problem::Reverted));
    }

    if root(&catalogue.files) != catalogue.root {
        problems.push(Problem::Root);
    }

    for entry in &catalogue.files {
        check_relative(&entry.path)?;
        match hash_file(&dir.join(&entry.path)) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                problems.push(Problem::Missing(entry.path.clone()))
            }
            Err(e) => return Err(e).with_context(|| format!("hashing {}", entry.path)),
            Ok((bytes, _)) if bytes != entry.bytes => problems.push(Problem::Size {
                path: entry.path.clone(),
                expected: entry.bytes,
                actual: bytes,
            }),
            Ok((_, sha256)) if sha256 != entry.sha256 => {
                problems.push(Problem::Hash(entry.path.clone()))
            }
            Ok(_) => {}
        }
    }

    // metadata.json is what restore reads, so every file it names must be
    // covered. Skipped when metadata.json itself failed above.
    let metadata_ok = !problems.iter().any(|p| match p {
        Problem::Missing(p) | Problem::Hash(p) | Problem::Size { path: p, .. } => {
            p == METADATA_FILE
        }
        _ => false,
    });
    if metadata_ok && catalogue.files.iter().any(|f| f.path == METADATA_FILE) {
        let metadata = read_metadata(dir)?;
        for path in dump_files(dir, &metadata)? {
            if !catalogue.files.iter().any(|f| f.path == path) {
                problems.push(Problem::Unlisted(path));
            }
        }
    } else if metadata_ok {
        problems.push(Problem::Missing(METADATA_FILE.to_string()));
    }

    Ok((catalogue, problems))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn head_hash(head: i32) -> String {
        format!("{head:064x}")
    }

    /// Metadata as graphman leaves it after `runs` dumps into one directory.
    fn metadata(head: i32, runs: usize) -> String {
        let chunks = |table: &str, kind: &str, n: usize| -> Vec<serde_json::Value> {
            (0..n)
                .map(|i| {
                    json!({ "file": format!("{table}/{kind}_{i:06}.parquet"),
                                 "min_vid": i, "max_vid": i, "row_count": 1 })
                })
                .collect()
        };
        json!({
            "version": 1,
            "deployment": "QmTest",
            "network": "mainnet",
            "manifest": { "spec_version": "1.0.0", "features": [],
                          "entities_with_causality_region": [], "history_blocks": 2147483647 },
            "earliest_block_number": 10,
            "start_block": { "number": 10, "hash": "0a" },
            "head_block": { "number": head, "hash": head_hash(head) },
            "entity_count": 3,
            "graft_base": null, "graft_block": null, "debug_fork": null,
            "health": { "failed": false, "health": "healthy", "fatal_error": null,
                        "non_fatal_errors": [] },
            "indexes": {},
            "tables": {
                "Token": { "immutable": false, "has_causality_region": false, "max_vid": runs,
                           "chunks": chunks("Token", "chunk", runs),
                           "clamps": chunks("Token", "clamp", runs - 1) },
                "Poi$": { "immutable": false, "has_causality_region": false, "max_vid": runs,
                          "chunks": chunks("Poi$", "chunk", runs) }
            }
        })
        .to_string()
    }

    /// Bring `dir` to the state after `runs` dumps, the last at `head`.
    fn dump_into(dir: &Path, head: i32, runs: usize) {
        fs::write(dir.join("metadata.json"), metadata(head, runs)).unwrap();
        fs::write(dir.join("schema.graphql"), "type Token @entity { id: ID! }").unwrap();
        fs::create_dir_all(dir.join("Token")).unwrap();
        fs::create_dir_all(dir.join("Poi$")).unwrap();
        for i in 0..runs {
            for stem in ["Token/chunk", "Poi$/chunk", "Token/clamp"] {
                let path = dir.join(format!("{stem}_{i:06}.parquet"));
                if !path.exists() && !(stem == "Token/clamp" && i == runs - 1) {
                    fs::write(path, format!("{stem} {i}")).unwrap();
                }
            }
        }
    }

    fn dump() -> TempDir {
        let dir = TempDir::new().unwrap();
        dump_into(dir.path(), 99, 1);
        dir
    }

    struct FakeChain(HashMap<i32, String>);

    impl FakeChain {
        fn with(heads: &[i32]) -> Self {
            Self(
                heads
                    .iter()
                    .map(|h| (*h, format!("0x{}", head_hash(*h))))
                    .collect(),
            )
        }
    }

    impl Chain for FakeChain {
        fn block_hash(&self, number: i32) -> Result<Option<String>> {
            Ok(self.0.get(&number).cloned())
        }
    }

    fn seal_on(dir: &Path, chain: &FakeChain) -> Result<Catalogue> {
        seal(
            dir,
            SealOptions {
                chain: Some(chain),
                ..Default::default()
            },
        )
    }

    #[test]
    fn seal_covers_chunks_and_metadata() {
        let dir = dump();
        let catalogue = seal(dir.path(), SealOptions::default()).unwrap();
        let paths: Vec<_> = catalogue.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Poi$/chunk_000000.parquet",
                "Token/chunk_000000.parquet",
                "metadata.json",
                "schema.graphql",
            ]
        );
        assert_eq!(catalogue.head_block.number, 99);
        assert_eq!(catalogue.earliest_block_number, 10);
        assert_eq!(catalogue.layers.len(), 1);
        assert_eq!(read_catalogue(dir.path()).unwrap(), catalogue);
    }

    #[test]
    fn sealed_dump_verifies() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        let (_, problems) = verify(dir.path(), None).unwrap();
        assert_eq!(problems, []);
    }

    #[test]
    fn seal_is_deterministic() {
        let a = seal(dump().path(), SealOptions::default()).unwrap();
        let b = seal(dump().path(), SealOptions::default()).unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn resealing_an_unchanged_dump_adds_no_layer() {
        let dir = dump();
        let a = seal(dir.path(), SealOptions::default()).unwrap();
        let b = seal(dir.path(), SealOptions::default()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn incremental_dump_seals_as_a_second_layer() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        dump_into(dir.path(), 150, 2);
        let catalogue = seal(dir.path(), SealOptions::default()).unwrap();
        let heads: Vec<_> = catalogue
            .layers
            .iter()
            .map(|l| l.head_block.number)
            .collect();
        assert_eq!(heads, [99, 150]);
        assert_eq!(
            catalogue.layers[1].files,
            [
                "Poi$/chunk_000001.parquet",
                "Token/chunk_000001.parquet",
                "Token/clamp_000000.parquet",
            ]
        );
        let (_, problems) = verify(dir.path(), Some(&FakeChain::with(&[99, 150]))).unwrap();
        assert_eq!(problems, []);
    }

    #[test]
    fn a_dump_nobody_sealed_is_refused() {
        let dir = TempDir::new().unwrap();
        dump_into(dir.path(), 150, 2);
        let err = seal(dir.path(), SealOptions::default()).unwrap_err();
        assert!(err.to_string().contains("unsealed"), "{err}");

        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        dump_into(dir.path(), 200, 3);
        let err = seal(dir.path(), SealOptions::default()).unwrap_err();
        assert!(err.to_string().contains("unsealed"), "{err}");
    }

    #[test]
    fn a_layer_from_a_reverted_fork_is_refused() {
        let dir = dump();
        seal_on(dir.path(), &FakeChain::with(&[99])).unwrap();
        dump_into(dir.path(), 150, 2);

        // Block 99 now has another hash: the first layer's fork is gone.
        let mut chain = FakeChain::with(&[150]);
        chain.0.insert(99, "0xdead".into());
        let err = seal_on(dir.path(), &chain).unwrap_err();
        assert!(err.to_string().contains("block 99"), "{err}");
        // The refused seal must not have replaced the catalogue.
        assert_eq!(read_catalogue(dir.path()).unwrap().layers.len(), 1);

        let (_, problems) = verify(dir.path(), Some(&chain)).unwrap();
        assert!(problems.contains(&Problem::Reverted(BlockPtr {
            number: 99,
            hash: head_hash(99)
        })));
    }

    #[test]
    fn a_head_beyond_the_chain_is_reverted() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        let (_, problems) = verify(dir.path(), Some(&FakeChain::with(&[]))).unwrap();
        assert!(matches!(problems[..], [Problem::Reverted(_)]));
    }

    #[test]
    fn a_sealed_file_may_not_change_before_the_next_seal() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        dump_into(dir.path(), 150, 2);
        fs::write(dir.path().join("Token/chunk_000000.parquet"), b"other").unwrap();
        let err = seal(dir.path(), SealOptions::default()).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
    }

    #[test]
    fn same_length_corruption_is_a_hash_problem() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        let path = dir.path().join("Token/chunk_000000.parquet");
        let len = fs::read(&path).unwrap().len();
        fs::write(&path, vec![b'z'; len]).unwrap();
        let (_, problems) = verify(dir.path(), None).unwrap();
        assert_eq!(
            problems,
            [Problem::Hash("Token/chunk_000000.parquet".into())]
        );
    }

    #[test]
    fn truncation_and_removal_are_reported() {
        let dir = dump();
        let catalogue = seal(dir.path(), SealOptions::default()).unwrap();
        let expected = catalogue.files[1].bytes;
        fs::write(dir.path().join("Token/chunk_000000.parquet"), b"to").unwrap();
        fs::remove_file(dir.path().join("Poi$/chunk_000000.parquet")).unwrap();
        let (_, problems) = verify(dir.path(), None).unwrap();
        assert_eq!(
            problems,
            [
                Problem::Missing("Poi$/chunk_000000.parquet".into()),
                Problem::Size {
                    path: "Token/chunk_000000.parquet".into(),
                    expected,
                    actual: 2
                },
            ]
        );
    }

    #[test]
    fn edited_file_list_breaks_the_root() {
        let dir = dump();
        let mut catalogue = seal(dir.path(), SealOptions::default()).unwrap();
        catalogue
            .files
            .retain(|f| f.path != "Token/chunk_000000.parquet");
        fs::write(
            dir.path().join(CATALOGUE_FILE),
            serde_json::to_vec(&catalogue).unwrap(),
        )
        .unwrap();
        let (_, problems) = verify(dir.path(), None).unwrap();
        assert_eq!(
            problems,
            [
                Problem::Root,
                Problem::Unlisted("Token/chunk_000000.parquet".into())
            ]
        );
    }

    #[test]
    fn paths_may_not_leave_the_dump() {
        let dir = dump();
        let evil = metadata(99, 1).replace("Token/chunk_000000.parquet", "../outside.parquet");
        fs::write(dir.path().join("metadata.json"), evil).unwrap();
        let err = seal(dir.path(), SealOptions::default()).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn headless_dump_is_refused() {
        let dir = dump();
        let mut headless: serde_json::Value = serde_json::from_str(&metadata(99, 1)).unwrap();
        headless["head_block"] = serde_json::Value::Null;
        fs::write(dir.path().join("metadata.json"), headless.to_string()).unwrap();
        assert!(seal(dir.path(), SealOptions::default()).is_err());
    }
}
