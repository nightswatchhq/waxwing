use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    /// Sorted by path.
    pub files: Vec<FileEntry>,
    /// SHA-256 over the `files` list; identifies this artefact, not the
    /// deployment's state. Parquet bytes differ between publishers.
    pub root: String,
}

#[derive(Debug, Default)]
pub struct SealOptions {
    pub graph_node_version: Option<String>,
    pub public_poi: Option<String>,
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

/// Hash every file a dump's `metadata.json` references and write
/// `catalogue.json` beside it.
pub fn seal(dir: &Path, opts: SealOptions) -> Result<Catalogue> {
    let metadata = read_metadata(dir)?;
    let Some(head_block) = metadata.head_block.clone() else {
        bail!("dump has no head block, nothing to seal");
    };

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
/// the publisher's data was right.
pub fn verify(dir: &Path) -> Result<(Catalogue, Vec<Problem>)> {
    let catalogue = read_catalogue(dir)?;
    let mut problems = Vec::new();

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
    use tempfile::TempDir;

    const METADATA: &str = r#"{
        "version": 1,
        "deployment": "QmTest",
        "network": "mainnet",
        "manifest": { "spec_version": "1.0.0", "features": [],
                      "entities_with_causality_region": [], "history_blocks": 2147483647 },
        "earliest_block_number": 10,
        "start_block": { "number": 10, "hash": "0x0a" },
        "head_block": { "number": 99, "hash": "0xdef" },
        "entity_count": 3,
        "graft_base": null, "graft_block": null, "debug_fork": null,
        "health": { "failed": false, "health": "healthy", "fatal_error": null, "non_fatal_errors": [] },
        "indexes": {},
        "tables": {
            "Token": { "immutable": false, "has_causality_region": false, "max_vid": 2,
                "chunks": [ { "file": "Token/chunk_000000.parquet", "min_vid": 0, "max_vid": 2, "row_count": 3 } ],
                "clamps": [ { "file": "Token/clamp_000000.parquet", "min_vid": 0, "max_vid": 1, "row_count": 1 } ] },
            "Poi$": { "immutable": false, "has_causality_region": false, "max_vid": 0,
                "chunks": [ { "file": "Poi$/chunk_000000.parquet", "min_vid": 0, "max_vid": 0, "row_count": 1 } ] }
        }
    }"#;

    fn dump() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        fs::write(p.join("metadata.json"), METADATA).unwrap();
        fs::write(p.join("schema.graphql"), "type Token @entity { id: ID! }").unwrap();
        fs::create_dir(p.join("Token")).unwrap();
        fs::create_dir(p.join("Poi$")).unwrap();
        fs::write(p.join("Token/chunk_000000.parquet"), b"tokens").unwrap();
        fs::write(p.join("Token/clamp_000000.parquet"), b"clamps").unwrap();
        fs::write(p.join("Poi$/chunk_000000.parquet"), b"poi").unwrap();
        dir
    }

    #[test]
    fn seal_covers_chunks_clamps_and_metadata() {
        let dir = dump();
        let catalogue = seal(dir.path(), SealOptions::default()).unwrap();
        let paths: Vec<_> = catalogue.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Poi$/chunk_000000.parquet",
                "Token/chunk_000000.parquet",
                "Token/clamp_000000.parquet",
                "metadata.json",
                "schema.graphql",
            ]
        );
        assert_eq!(catalogue.head_block.number, 99);
        assert_eq!(catalogue.earliest_block_number, 10);
        assert_eq!(read_catalogue(dir.path()).unwrap(), catalogue);
    }

    #[test]
    fn sealed_dump_verifies() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        let (_, problems) = verify(dir.path()).unwrap();
        assert_eq!(problems, []);
    }

    #[test]
    fn seal_is_deterministic() {
        let a = seal(dump().path(), SealOptions::default()).unwrap();
        let b = seal(dump().path(), SealOptions::default()).unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn same_length_corruption_is_a_hash_problem() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        fs::write(dir.path().join("Token/chunk_000000.parquet"), b"tokenz").unwrap();
        let (_, problems) = verify(dir.path()).unwrap();
        assert_eq!(
            problems,
            [Problem::Hash("Token/chunk_000000.parquet".into())]
        );
    }

    #[test]
    fn truncation_and_removal_are_reported() {
        let dir = dump();
        seal(dir.path(), SealOptions::default()).unwrap();
        fs::write(dir.path().join("Token/clamp_000000.parquet"), b"cl").unwrap();
        fs::remove_file(dir.path().join("Poi$/chunk_000000.parquet")).unwrap();
        let (_, problems) = verify(dir.path()).unwrap();
        assert_eq!(
            problems,
            [
                Problem::Missing("Poi$/chunk_000000.parquet".into()),
                Problem::Size {
                    path: "Token/clamp_000000.parquet".into(),
                    expected: 6,
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
            .retain(|f| f.path != "Token/clamp_000000.parquet");
        fs::write(
            dir.path().join(CATALOGUE_FILE),
            serde_json::to_vec(&catalogue).unwrap(),
        )
        .unwrap();
        let (_, problems) = verify(dir.path()).unwrap();
        assert_eq!(
            problems,
            [
                Problem::Root,
                Problem::Unlisted("Token/clamp_000000.parquet".into())
            ]
        );
    }

    #[test]
    fn paths_may_not_leave_the_dump() {
        let dir = dump();
        let evil = METADATA.replace("Token/chunk_000000.parquet", "../outside.parquet");
        fs::write(dir.path().join("metadata.json"), evil).unwrap();
        let err = seal(dir.path(), SealOptions::default()).unwrap_err();
        assert!(err.to_string().contains("escapes"), "{err}");
    }

    #[test]
    fn headless_dump_is_refused() {
        let dir = dump();
        let headless = METADATA.replace(r#"{ "number": 99, "hash": "0xdef" }"#, "null");
        fs::write(dir.path().join("metadata.json"), headless).unwrap();
        assert!(seal(dir.path(), SealOptions::default()).is_err());
    }
}
