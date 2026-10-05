use std::fs::{self, File};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, BooleanArray, Int32Array, RecordBatch};
use arrow::compute::filter_record_batch;
use arrow::datatypes::Int64Type;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde_json::json;

use super::diff::{batches, clamps, int32};
use super::{
    BlockPtr, CATALOGUE_FILE, Chain, DumpTable, Layer, check_relative, read_catalogue,
    read_metadata, reverted_layers,
};

const DATA_SOURCES_TABLE: &str = "data_sources$";
const CHUNK: &str = "chunk_000000.parquet";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cut {
    pub block: BlockPtr,
    /// Versions written.
    pub versions: usize,
    /// Versions that began after the block, and were left out.
    pub dropped: usize,
    /// Versions closed after the block, written open.
    pub reopened: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutAt {
    Block(i32),
    /// The chain's finalized block, or the dump head if that is lower.
    Final,
}

/// Decide the block to cut `src` at, and learn its hash.
///
/// With a chain, the dump's head and every sealed layer must be on it, so
/// the block at any lower height is an ancestor of what was indexed and
/// the chain's hash for it is the right one. Without, only a block the
/// dump was sealed at will do, since the catalogue recorded its hash.
pub fn cut_block(src: &Path, at: CutAt, chain: Option<&dyn Chain>) -> Result<BlockPtr> {
    let metadata = read_metadata(src)?;
    let Some(head) = metadata.head_block else {
        bail!("dump has no head block");
    };
    let mut layers = match src.join(CATALOGUE_FILE).exists() {
        true => read_catalogue(src)?.layers,
        false => Vec::new(),
    };

    let Some(chain) = chain else {
        let sealed = layers.into_iter().map(|l| l.head_block);
        return match at {
            CutAt::Block(number) => sealed.clone().find(|h| h.number == number).with_context(|| {
                format!("block {number} is not one the dump was sealed at: pass --rpc to learn its hash")
            }),
            CutAt::Final => bail!("finding the finalized block needs --rpc"),
        };
    };

    layers.push(Layer {
        head_block: head.clone(),
        files: Vec::new(),
    });
    if let Some(gone) = reverted_layers(&layers, chain)?.first() {
        bail!(
            "the dump was taken at block {} ({}), which is not on this chain",
            gone.number,
            gone.hash
        );
    }
    let number = match at {
        CutAt::Block(number) => number,
        CutAt::Final => chain
            .finalized()?
            .context("the chain reports no finalized block")?
            .min(head.number),
    };
    if number == head.number {
        return Ok(head);
    }
    let hash = chain
        .block_hash(number)?
        .with_context(|| format!("the chain has no block {number}"))?;
    Ok(BlockPtr { number, hash })
}

/// The block a dump was taken at.
pub fn dump_head(dir: &Path) -> Result<BlockPtr> {
    read_metadata(dir)?
        .head_block
        .context("dump has no head block")
}

#[derive(Default)]
struct TableCut {
    versions: usize,
    dropped: usize,
    reopened: usize,
    open: usize,
    min_vid: i64,
    max_vid: i64,
}

fn cut_table(src: &Path, dst: &Path, name: &str, table: &DumpTable, cut: i32) -> Result<TableCut> {
    let clamps = clamps(src, table)?;
    let mut out = TableCut {
        max_vid: -1,
        ..Default::default()
    };
    let mut writer = None;

    for chunk in &table.chunks {
        check_relative(&chunk.file)?;
        for batch in batches(&src.join(&chunk.file))? {
            let batch = batch?;
            let schema = batch.schema();
            let vids = batch
                .column_by_name("vid")
                .and_then(|c| c.as_primitive_opt::<Int64Type>())
                .with_context(|| format!("{} has no Int64 vid", chunk.file))?;
            let starts = match schema.column_with_name("block$") {
                Some(_) => int32(&batch, "block$")?,
                None => int32(&batch, "block_range_start")?,
            };
            let keep: BooleanArray = starts.values().iter().map(|s| Some(*s <= cut)).collect();
            out.dropped += keep.false_count();

            let mut columns: Vec<ArrayRef> = batch.columns().to_vec();
            let end_column = schema.column_with_name("block_range_end").map(|(i, _)| i);
            if let Some(index) = end_column {
                let ends = int32(&batch, "block_range_end")?;
                let cut_ends: Int32Array = (0..batch.num_rows())
                    .map(|i| {
                        let end = clamps
                            .get(&vids.value(i))
                            .copied()
                            .or_else(|| ends.is_valid(i).then(|| ends.value(i)));
                        if keep.value(i) && end.is_some_and(|end| end > cut) {
                            out.reopened += 1;
                        }
                        end.filter(|end| *end <= cut)
                    })
                    .collect();
                columns[index] = Arc::new(cut_ends);
            }
            let kept = filter_record_batch(&RecordBatch::try_new(schema.clone(), columns)?, &keep)?;
            if kept.num_rows() == 0 {
                continue;
            }

            let vids = kept
                .column_by_name("vid")
                .unwrap()
                .as_primitive::<Int64Type>();
            if out.versions == 0 {
                out.min_vid = vids.value(0);
            }
            out.max_vid = vids.value(kept.num_rows() - 1);
            out.versions += kept.num_rows();
            out.open += match end_column {
                Some(index) => kept.column(index).null_count(),
                None => kept.num_rows(),
            };

            if writer.is_none() {
                fs::create_dir_all(dst.join(name))?;
                let properties = WriterProperties::builder()
                    .set_compression(Compression::ZSTD(ZstdLevel::default()))
                    .build();
                let file = File::create(dst.join(name).join(CHUNK))?;
                writer = Some(ArrowWriter::try_new(file, schema, Some(properties))?);
            }
            writer.as_mut().unwrap().write(&kept)?;
        }
    }
    if let Some(writer) = writer {
        writer.close()?;
    }
    Ok(out)
}

/// Write to `dst` the dump `src` would have been had it been taken at
/// `block`: later versions dropped, later closes undone, clamp files
/// folded in. `graphman restore` of the result is a deployment rewound to
/// that block, without the source deployment being touched.
///
/// The caller vouches that `block.hash` is the block the source indexed at
/// that height.
pub fn cut(src: &Path, dst: &Path, block: BlockPtr) -> Result<Cut> {
    let metadata = read_metadata(src)?;
    let Some(head) = &metadata.head_block else {
        bail!("dump has no head block");
    };
    if block.number > head.number {
        bail!(
            "block {} is beyond the dump head {}",
            block.number,
            head.number
        );
    }
    if block.number < metadata.earliest_block_number {
        bail!(
            "block {} is before the dump's earliest block {}",
            block.number,
            metadata.earliest_block_number
        );
    }
    if let Some(graft) = &metadata.graft_block
        && block.number < graft.number
    {
        bail!(
            "block {} is below the graft point {}",
            block.number,
            graft.number
        );
    }
    if dst.exists() && fs::read_dir(dst)?.next().is_some() {
        bail!("{} is not empty", dst.display());
    }
    fs::create_dir_all(dst)?;

    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(src.join("metadata.json"))?)?;
    let mut result = Cut {
        block: BlockPtr {
            number: block.number,
            hash: block.hash.trim_start_matches("0x").to_ascii_lowercase(),
        },
        versions: 0,
        dropped: 0,
        reopened: 0,
    };
    let mut entity_count = 0;

    for (name, table) in &metadata.tables {
        let table_cut = cut_table(src, dst, name, table, block.number)
            .with_context(|| format!("cutting {name}"))?;
        result.versions += table_cut.versions;
        result.dropped += table_cut.dropped;
        result.reopened += table_cut.reopened;
        // graph-node's count is of live entities, the POI's among them.
        if name != DATA_SOURCES_TABLE {
            entity_count += table_cut.open;
        }

        let entry = &mut raw["tables"][name];
        entry["chunks"] = if table_cut.versions == 0 {
            json!([])
        } else {
            json!([{
                "file": format!("{name}/{CHUNK}"),
                "min_vid": table_cut.min_vid,
                "max_vid": table_cut.max_vid,
                "row_count": table_cut.versions,
            }])
        };
        entry["clamps"] = json!([]);
        entry["max_vid"] = json!(table_cut.max_vid);
    }

    raw["head_block"] = json!({ "number": result.block.number, "hash": result.block.hash });
    raw["entity_count"] = json!(entity_count);

    for file in ["schema.graphql", "subgraph.yaml"] {
        if src.join(file).exists() {
            fs::copy(src.join(file), dst.join(file))?;
        }
    }
    // Last, as graphman does: a directory without it is not yet a dump.
    let tmp = dst.join("metadata.json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(&raw)?)?;
    fs::rename(&tmp, dst.join("metadata.json"))?;
    debug_assert!(!dst.join(CATALOGUE_FILE).exists());
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::tests::{BASE, TokenRow, dump};
    use crate::tests::FakeChain;
    use crate::{SealOptions, diff, seal, state};
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn ptr(number: i32) -> BlockPtr {
        BlockPtr {
            number,
            hash: "0xBEEF".into(),
        }
    }

    /// BASE plus: "b" closed at 90 by a clamp file and replaced, "c" born at 70.
    fn source() -> TempDir {
        let mut rows: Vec<TokenRow> = BASE.to_vec();
        rows.push((4, 70, None, "c", 3));
        rows.push((5, 90, None, "b", 9));
        dump(100, &rows, &[(3, 90)])
    }

    fn metadata(dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(dir.join("metadata.json")).unwrap()).unwrap()
    }

    #[test]
    fn cut_is_the_dump_as_it_stood_at_the_block() {
        let src = source();
        let dst = TempDir::new().unwrap();
        let result = cut(src.path(), dst.path(), ptr(60)).unwrap();
        assert_eq!(
            (result.versions, result.dropped, result.reopened),
            (3, 2, 1)
        );

        assert_eq!(
            state(dst.path(), None).unwrap(),
            state(src.path(), Some(60)).unwrap()
        );
        assert_eq!(diff(dst.path(), src.path(), None).unwrap().first(), None);

        let metadata = metadata(dst.path());
        assert_eq!(
            metadata["head_block"],
            json!({ "number": 60, "hash": "beef" })
        );
        // "a" and "b" are live at 60; the closed first version of "a" is not.
        assert_eq!(metadata["entity_count"], 2);
        let table = &metadata["tables"]["Token"];
        assert_eq!(table["clamps"], json!([]));
        assert_eq!(table["max_vid"], 3);
        assert_eq!(
            table["chunks"],
            json!([{ "file": "Token/chunk_000000.parquet", "min_vid": 1, "max_vid": 3, "row_count": 3 }])
        );
    }

    /// A chain holding the test dump's head (block 100, hash "aa").
    fn chain(finalized: Option<i32>) -> FakeChain {
        let blocks = [(100, "0xAA"), (60, "0x60"), (30, "0x30")];
        let blocks: HashMap<_, _> = blocks
            .into_iter()
            .map(|(n, h)| (n, h.to_string()))
            .collect();
        FakeChain(blocks, finalized)
    }

    #[test]
    fn final_is_the_finalized_block_but_never_past_the_head() {
        let src = source();
        let block = cut_block(src.path(), CutAt::Final, Some(&chain(Some(60)))).unwrap();
        assert_eq!((block.number, block.hash.as_str()), (60, "0x60"));

        let block = cut_block(src.path(), CutAt::Final, Some(&chain(Some(500)))).unwrap();
        assert_eq!((block.number, block.hash.as_str()), (100, "aa"));

        assert!(cut_block(src.path(), CutAt::Final, Some(&chain(None))).is_err());
        assert!(cut_block(src.path(), CutAt::Final, None).is_err());
    }

    #[test]
    fn a_dump_from_another_fork_is_not_cut() {
        let src = source();
        let mut chain = chain(Some(60));
        chain.0.insert(100, "0xdead".into());
        let err = cut_block(src.path(), CutAt::Block(60), Some(&chain)).unwrap_err();
        assert!(err.to_string().contains("not on this chain"), "{err}");
    }

    #[test]
    fn without_a_chain_only_a_sealed_head_has_a_known_hash() {
        let src = dump(100, &BASE, &[]);
        assert!(cut_block(src.path(), CutAt::Block(100), None).is_err());
        fs::write(src.path().join("schema.graphql"), "").unwrap();
        seal(src.path(), SealOptions::default()).unwrap();
        assert_eq!(
            cut_block(src.path(), CutAt::Block(100), None)
                .unwrap()
                .number,
            100
        );
        assert!(cut_block(src.path(), CutAt::Block(60), None).is_err());
    }

    #[test]
    fn a_sealed_state_root_is_checked_against_the_rows() {
        let src = dump(100, &BASE, &[]);
        fs::write(src.path().join("schema.graphql"), "").unwrap();
        let options = SealOptions {
            state: true,
            ..Default::default()
        };
        let mut catalogue = seal(src.path(), options).unwrap();
        assert_eq!(
            catalogue.state_root,
            Some(state(src.path(), None).unwrap().root)
        );
        assert_eq!(crate::verify(src.path(), None).unwrap().1, []);

        catalogue.state_root = Some("00".into());
        let json = serde_json::to_vec(&catalogue).unwrap();
        fs::write(src.path().join(CATALOGUE_FILE), json).unwrap();
        assert_eq!(
            crate::verify(src.path(), None).unwrap().1,
            [crate::Problem::State]
        );
    }

    #[test]
    fn cut_at_the_head_only_folds_the_clamps_in() {
        let src = source();
        let dst = TempDir::new().unwrap();
        let result = cut(src.path(), dst.path(), ptr(100)).unwrap();
        assert_eq!(
            (result.versions, result.dropped, result.reopened),
            (5, 0, 0)
        );
        assert_eq!(
            state(dst.path(), None).unwrap(),
            state(src.path(), None).unwrap()
        );
    }

    #[test]
    fn a_table_cut_to_nothing_has_no_chunks() {
        let src = source();
        let dst = TempDir::new().unwrap();
        cut(src.path(), dst.path(), ptr(5)).unwrap();
        let metadata = metadata(dst.path());
        assert_eq!(metadata["tables"]["Token"]["chunks"], json!([]));
        assert_eq!(metadata["tables"]["Token"]["max_vid"], -1);
        assert_eq!(metadata["entity_count"], 0);
        assert!(!dst.path().join("Token").exists());
    }

    #[test]
    fn cut_refuses_a_block_past_the_head_and_a_used_directory() {
        let src = source();
        let dst = TempDir::new().unwrap();
        assert!(cut(src.path(), dst.path(), ptr(101)).is_err());
        cut(src.path(), dst.path(), ptr(60)).unwrap();
        let err = cut(src.path(), dst.path(), ptr(60)).unwrap_err();
        assert!(err.to_string().contains("not empty"), "{err}");
    }
}
