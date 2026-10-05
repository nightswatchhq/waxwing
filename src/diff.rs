use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, PrimitiveArray, RecordBatch};
use arrow::datatypes::{DataType, Int32Type, Int64Type};
use arrow::row::{RowConverter, SortField};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use sha2::{Digest, Sha256};

use super::{DumpTable, check_relative, read_metadata};

const SAMPLES: usize = 3;

/// Every column but `vid` and the upper block bound. `vid` is a storage
/// detail, and the upper bound is the one thing a version changes later.
type Key = [u8; 32];
type Shape = Vec<(String, DataType)>;

struct Version {
    start: i32,
    /// More than one only if the dump holds the same version twice.
    ends: Vec<Option<i32>>,
}

struct Row<'a> {
    key: Key,
    start: i32,
    end: Option<i32>,
    batch: &'a RecordBatch,
    index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDiff {
    pub table: String,
    /// Entity versions on each side, as of the compared block.
    pub versions: [usize; 2],
    /// Versions the other side does not have at all.
    pub only: [usize; 2],
    /// Versions both have, but closed at different blocks.
    pub closed_differently: usize,
    pub first_block: Option<i32>,
    /// A few of the versions that differ at `first_block`.
    pub samples: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub deployment: String,
    /// Both dumps are compared as they stood at this block.
    pub block: i32,
    pub tables: Vec<TableDiff>,
}

impl Diff {
    /// The earliest block at which the two dumps disagree, and where.
    pub fn first(&self) -> Option<(i32, &str)> {
        self.tables
            .iter()
            .filter_map(|t| Some((t.first_block?, t.table.as_str())))
            .min()
    }
}

pub(super) fn batches(path: &Path) -> Result<impl Iterator<Item = Result<RecordBatch>>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    Ok(reader.map(|batch| Ok(batch?)))
}

pub(super) fn int32<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<&'a PrimitiveArray<Int32Type>> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_primitive_opt::<Int32Type>())
        .with_context(|| format!("no Int32 column {name}"))
}

/// Upper block bounds recorded by clamp files, by `vid`: versions closed
/// after the chunk holding them was written.
pub(super) fn clamps(dir: &Path, table: &DumpTable) -> Result<HashMap<i64, i32>> {
    let mut clamps = HashMap::new();
    for clamp in &table.clamps {
        check_relative(&clamp.file)?;
        for batch in batches(&dir.join(&clamp.file))? {
            let batch = batch?;
            let vids = batch
                .column_by_name("vid")
                .and_then(|c| c.as_primitive_opt::<Int64Type>())
                .context("clamp file has no Int64 vid")?;
            let ends = int32(&batch, "block_range_end")?;
            for i in 0..batch.num_rows() {
                clamps.insert(vids.value(i), ends.value(i));
            }
        }
    }
    Ok(clamps)
}

/// Call `f` for every version in a table as it stood at block `cut`:
/// clamp files applied, later versions dropped, later closes undone.
fn visit(
    dir: &Path,
    table: &DumpTable,
    cut: i32,
    shape: &mut Option<Shape>,
    mut f: impl FnMut(Row) -> Result<()>,
) -> Result<()> {
    let clamps = clamps(dir, table)?;

    for chunk in &table.chunks {
        check_relative(&chunk.file)?;
        for batch in batches(&dir.join(&chunk.file))? {
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
            let ends = match schema.column_with_name("block_range_end") {
                Some(_) => Some(int32(&batch, "block_range_end")?),
                None => None,
            };

            let mut fields = Vec::new();
            let mut columns: Vec<ArrayRef> = Vec::new();
            let mut this_shape = Shape::new();
            for (field, column) in schema.fields().iter().zip(batch.columns()) {
                if field.name() == "vid" || field.name() == "block_range_end" {
                    continue;
                }
                fields.push(SortField::new(field.data_type().clone()));
                columns.push(column.clone());
                this_shape.push((field.name().clone(), field.data_type().clone()));
            }
            match shape {
                None => *shape = Some(this_shape),
                Some(shape) if *shape != this_shape => {
                    bail!(
                        "{} does not have the columns of the other chunks",
                        chunk.file
                    )
                }
                Some(_) => {}
            }
            let rows = RowConverter::new(fields)?.convert_columns(&columns)?;

            for index in 0..batch.num_rows() {
                let start = starts.value(index);
                if start > cut {
                    continue;
                }
                let end = clamps
                    .get(&vids.value(index))
                    .copied()
                    .or_else(|| ends.and_then(|e| e.is_valid(index).then(|| e.value(index))))
                    .filter(|end| *end <= cut);
                f(Row {
                    key: Sha256::digest(rows.row(index).as_ref()).into(),
                    start,
                    end,
                    batch: &batch,
                    index,
                })?;
            }
        }
    }
    Ok(())
}

fn load(
    dir: &Path,
    table: &DumpTable,
    cut: i32,
    shape: &mut Option<Shape>,
) -> Result<HashMap<Key, Version>> {
    let mut versions: HashMap<Key, Version> = HashMap::new();
    visit(dir, table, cut, shape, |row| {
        let version = versions.entry(row.key).or_insert(Version {
            start: row.start,
            ends: Vec::new(),
        });
        version.ends.push(row.end);
        Ok(())
    })?;
    for version in versions.values_mut() {
        version.ends.sort();
    }
    Ok(versions)
}

fn describe(row: &Row) -> Result<String> {
    let options = FormatOptions::default().with_null("null");
    let mut out = match row.end {
        Some(end) => format!("[{}, {end})", row.start),
        None => format!("[{}, )", row.start),
    };
    let schema = row.batch.schema();
    for (field, column) in schema.fields().iter().zip(row.batch.columns()) {
        if ["vid", "block$", "block_range_start", "block_range_end"]
            .contains(&field.name().as_str())
        {
            continue;
        }
        let value = ArrayFormatter::try_new(column.as_ref(), &options)?
            .value(row.index)
            .to_string();
        let value: String = value.chars().take(72).collect();
        out.push_str(&format!(" {}={value}", field.name()));
    }
    Ok(out)
}

fn samples(
    side: &str,
    dir: &Path,
    table: &DumpTable,
    cut: i32,
    keys: &HashSet<Key>,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if keys.is_empty() {
        return Ok(out);
    }
    visit(dir, table, cut, &mut None, |row| {
        if keys.contains(&row.key) && out.len() < SAMPLES {
            out.push(format!("{side} {}", describe(&row)?));
        }
        Ok(())
    })?;
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableState {
    pub table: String,
    pub versions: usize,
    pub root: String,
}

/// A commitment to a deployment's entity versions as of one block. Unlike
/// the catalogue root it does not depend on `vid`, row order or Parquet
/// encoding, so independent indexers can be expected to agree on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub deployment: String,
    pub block: i32,
    pub tables: Vec<TableState>,
    pub root: String,
}

/// Hash a dump's entity versions as they stood at `at` (default: its head).
pub fn state(dir: &Path, at: Option<i32>) -> Result<State> {
    let metadata = read_metadata(dir)?;
    let Some(head) = metadata.head_block.as_ref().map(|h| h.number) else {
        bail!("a dump without a head block has no state");
    };
    let block = at.unwrap_or(head);
    if block > head {
        bail!("block {block} is beyond the dump head {head}");
    }
    if block < metadata.earliest_block_number {
        bail!(
            "block {block} is before the dump's earliest block {}",
            metadata.earliest_block_number
        );
    }

    let mut all = Sha256::new();
    all.update(b"waxwing-state-v1\0");
    all.update(metadata.deployment.as_bytes());
    all.update(block.to_be_bytes());

    let mut tables = Vec::new();
    for (name, table) in &metadata.tables {
        let mut shape = None;
        let mut leaves: Vec<Key> = Vec::new();
        visit(dir, table, block, &mut shape, |row| {
            let mut leaf = Sha256::new();
            leaf.update(row.key);
            match row.end {
                Some(end) => leaf.update(end.to_be_bytes()),
                None => leaf.update(b"open"),
            }
            leaves.push(leaf.finalize().into());
            Ok(())
        })
        .with_context(|| format!("reading {name}"))?;
        leaves.sort_unstable();

        let mut hasher = Sha256::new();
        for (column, data_type) in shape.iter().flatten() {
            hasher.update(format!("{column}:{data_type}\0"));
        }
        for leaf in &leaves {
            hasher.update(leaf);
        }
        let root = hex::encode(hasher.finalize());
        all.update(format!("\0{name}\0{root}"));
        tables.push(TableState {
            table: name.clone(),
            versions: leaves.len(),
            root,
        });
    }

    Ok(State {
        deployment: metadata.deployment,
        block,
        tables,
        root: hex::encode(all.finalize()),
    })
}

/// Compare two dumps of one deployment, version by version, as both stood
/// at `at` (default: the lower of the two heads).
///
/// Holds a 32-byte key per version in memory, for both sides of one table
/// at a time.
pub fn diff(a: &Path, b: &Path, at: Option<i32>) -> Result<Diff> {
    let (meta_a, meta_b) = (read_metadata(a)?, read_metadata(b)?);
    if meta_a.deployment != meta_b.deployment {
        bail!(
            "different deployments: {} and {}",
            meta_a.deployment,
            meta_b.deployment
        );
    }
    let head = |m: &super::DumpMetadata| m.head_block.as_ref().map(|h| h.number);
    let (Some(head_a), Some(head_b)) = (head(&meta_a), head(&meta_b)) else {
        bail!("a dump without a head block cannot be compared");
    };
    let block = at.unwrap_or(head_a.min(head_b));
    if block > head_a.min(head_b) {
        bail!("block {block} is beyond a dump head ({head_a} and {head_b})");
    }
    // A pruned dump has dropped versions the other still holds, which would
    // all read as divergence.
    if meta_a.earliest_block_number != meta_b.earliest_block_number {
        bail!(
            "the dumps start at different blocks ({} and {}): one is pruned",
            meta_a.earliest_block_number,
            meta_b.earliest_block_number
        );
    }
    let names = |m: &super::DumpMetadata| m.tables.keys().cloned().collect::<BTreeSet<_>>();
    if names(&meta_a) != names(&meta_b) {
        bail!("the dumps do not have the same tables");
    }

    let mut tables = Vec::new();
    for (name, table_a) in &meta_a.tables {
        let table_b = &meta_b.tables[name];
        let mut shape = None;
        let versions_a =
            load(a, table_a, block, &mut shape).with_context(|| format!("reading {name} in A"))?;
        let versions_b =
            load(b, table_b, block, &mut shape).with_context(|| format!("reading {name} in B"))?;

        let mut table = TableDiff {
            table: name.clone(),
            versions: [versions_a.len(), versions_b.len()],
            only: [0, 0],
            closed_differently: 0,
            first_block: None,
            samples: Vec::new(),
        };
        // (block, key, in A, in B) for every version that differs
        let mut differing = Vec::new();
        for (key, version) in &versions_a {
            match versions_b.get(key) {
                None => {
                    table.only[0] += 1;
                    differing.push((version.start, *key, true, false));
                }
                Some(other) if other.ends != version.ends => {
                    table.closed_differently += 1;
                    let ends = version.ends.iter().chain(&other.ends);
                    let block = ends.flatten().min().copied().unwrap_or(version.start);
                    differing.push((block, *key, true, true));
                }
                Some(_) => {}
            }
        }
        for (key, version) in &versions_b {
            if !versions_a.contains_key(key) {
                table.only[1] += 1;
                differing.push((version.start, *key, false, true));
            }
        }

        table.first_block = differing.iter().map(|d| d.0).min();
        if let Some(first) = table.first_block {
            let at_first = |in_side: fn(&(i32, Key, bool, bool)) -> bool| -> HashSet<Key> {
                differing
                    .iter()
                    .filter(|d| d.0 == first && in_side(d))
                    .map(|d| d.1)
                    .collect()
            };
            table.samples = samples("A", a, table_a, block, &at_first(|d| d.2))?;
            table
                .samples
                .extend(samples("B", b, table_b, block, &at_first(|d| d.3))?);
        }
        tables.push(table);
    }

    Ok(Diff {
        deployment: meta_a.deployment,
        block,
        tables,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    use arrow::array::{Int32Array, Int64Array, StringArray};
    use arrow::datatypes::{Field, Schema};
    use parquet::arrow::ArrowWriter;
    use serde_json::json;
    use tempfile::TempDir;

    /// (vid, start, end, id, value)
    pub(crate) type TokenRow = (i64, i32, Option<i32>, &'static str, i32);

    fn write(path: &Path, schema: Schema, columns: Vec<ArrayRef>) {
        let schema = Arc::new(schema);
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let mut writer = ArrowWriter::try_new(File::create(path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    pub(crate) fn dump(head: i32, rows: &[TokenRow], clamps: &[(i64, i32)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("Token")).unwrap();
        write(
            &dir.path().join("Token/chunk_000000.parquet"),
            Schema::new(vec![
                Field::new("vid", DataType::Int64, false),
                Field::new("block_range_start", DataType::Int32, false),
                Field::new("block_range_end", DataType::Int32, true),
                Field::new("id", DataType::Utf8, false),
                Field::new("value", DataType::Int32, false),
            ]),
            vec![
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.0))),
                Arc::new(Int32Array::from_iter_values(rows.iter().map(|r| r.1))),
                Arc::new(Int32Array::from_iter(rows.iter().map(|r| r.2))),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.3))),
                Arc::new(Int32Array::from_iter_values(rows.iter().map(|r| r.4))),
            ],
        );
        let mut clamp_files = Vec::new();
        if !clamps.is_empty() {
            write(
                &dir.path().join("Token/clamp_000000.parquet"),
                Schema::new(vec![
                    Field::new("vid", DataType::Int64, false),
                    Field::new("block_range_end", DataType::Int32, false),
                ]),
                vec![
                    Arc::new(Int64Array::from_iter_values(clamps.iter().map(|c| c.0))),
                    Arc::new(Int32Array::from_iter_values(clamps.iter().map(|c| c.1))),
                ],
            );
            clamp_files.push(json!({ "file": "Token/clamp_000000.parquet" }));
        }
        let metadata = json!({
            "version": 1,
            "deployment": "QmTest",
            "network": "mainnet",
            "manifest": { "history_blocks": 2147483647 },
            "earliest_block_number": 0,
            "head_block": { "number": head, "hash": "aa" },
            "graft_base": null,
            "graft_block": null,
            "tables": { "Token": {
                "chunks": [ { "file": "Token/chunk_000000.parquet" } ],
                "clamps": clamp_files,
            } }
        });
        fs::write(dir.path().join("metadata.json"), metadata.to_string()).unwrap();
        dir
    }

    fn token(a: &TempDir, b: &TempDir) -> TableDiff {
        let diff = diff(a.path(), b.path(), None).unwrap();
        diff.tables.into_iter().next().unwrap()
    }

    pub(crate) const BASE: [TokenRow; 3] = [
        (1, 10, Some(50), "a", 1),
        (2, 50, None, "a", 2),
        (3, 20, None, "b", 7),
    ];

    #[test]
    fn same_versions_under_other_vids_and_order_are_identical() {
        let a = dump(100, &BASE, &[]);
        let b = dump(
            100,
            &[
                (70, 20, None, "b", 7),
                (80, 50, None, "a", 2),
                (90, 10, Some(50), "a", 1),
            ],
            &[],
        );
        let table = token(&a, &b);
        assert_eq!(table.first_block, None);
        assert_eq!(table.versions, [3, 3]);
    }

    #[test]
    fn a_version_only_one_side_has_diverges_where_it_starts() {
        let a = dump(100, &BASE, &[]);
        let mut rows = BASE.to_vec();
        rows.push((4, 64, None, "phantom", 0));
        let b = dump(100, &rows, &[]);
        let table = token(&a, &b);
        assert_eq!(table.only, [0, 1]);
        assert_eq!(table.first_block, Some(64));
        assert_eq!(table.samples, ["B [64, ) id=phantom value=0"]);
    }

    #[test]
    fn a_changed_value_shows_both_sides() {
        let a = dump(100, &BASE, &[]);
        let mut rows = BASE;
        rows[2].4 = 8;
        let b = dump(100, &rows, &[]);
        let table = token(&a, &b);
        assert_eq!(table.only, [1, 1]);
        assert_eq!(table.first_block, Some(20));
        assert_eq!(
            table.samples,
            ["A [20, ) id=b value=7", "B [20, ) id=b value=8"]
        );
    }

    #[test]
    fn a_version_closed_on_one_side_diverges_at_the_close() {
        let a = dump(100, &BASE, &[]);
        let mut rows = BASE;
        rows[2].2 = Some(70);
        let b = dump(100, &rows, &[]);
        let table = token(&a, &b);
        assert_eq!(table.closed_differently, 1);
        assert_eq!(table.first_block, Some(70));
    }

    #[test]
    fn a_clamp_file_closes_a_version() {
        let mut open = BASE;
        open[0].2 = None;
        let a = dump(100, &open, &[(1, 50)]);
        let b = dump(100, &BASE, &[]);
        assert_eq!(token(&a, &b).first_block, None);
    }

    #[test]
    fn a_longer_dump_is_cut_back_to_the_shorter_head() {
        let a = dump(80, &BASE, &[]);
        let mut rows = BASE.to_vec();
        rows[2].2 = Some(90);
        rows.push((4, 90, None, "b", 9));
        let b = dump(100, &rows, &[]);
        let diff = diff(a.path(), b.path(), None).unwrap();
        assert_eq!(diff.block, 80);
        assert_eq!(diff.first(), None);
        assert_eq!(diff.tables[0].versions, [3, 3]);
    }

    #[test]
    fn state_ignores_vid_order_and_how_a_close_was_recorded() {
        let a = dump(100, &BASE, &[]);
        let b = dump(
            100,
            &[
                (70, 20, None, "b", 7),
                (80, 50, None, "a", 2),
                (90, 10, None, "a", 1),
            ],
            &[(90, 50)],
        );
        let (a, b) = (
            state(a.path(), None).unwrap(),
            state(b.path(), None).unwrap(),
        );
        assert_eq!(a, b);
        assert_eq!(a.tables[0].versions, 3);
    }

    #[test]
    fn state_changes_with_a_value_a_close_or_the_block() {
        let base = state(dump(100, &BASE, &[]).path(), None).unwrap().root;

        let mut rows = BASE;
        rows[2].4 = 8;
        assert_ne!(
            state(dump(100, &rows, &[]).path(), None).unwrap().root,
            base
        );

        let mut rows = BASE;
        rows[2].2 = Some(70);
        assert_ne!(
            state(dump(100, &rows, &[]).path(), None).unwrap().root,
            base
        );

        assert_ne!(
            state(dump(101, &BASE, &[]).path(), None).unwrap().root,
            base
        );
    }

    #[test]
    fn state_of_a_longer_dump_cut_back_matches_the_shorter_one() {
        let short = dump(80, &BASE, &[]);
        let mut rows = BASE.to_vec();
        rows[2].2 = Some(90);
        rows.push((4, 90, None, "b", 9));
        let long = dump(100, &rows, &[]);
        assert_eq!(
            state(long.path(), Some(80)).unwrap(),
            state(short.path(), None).unwrap()
        );
        assert!(state(long.path(), Some(101)).is_err());
    }

    #[test]
    fn at_must_not_pass_either_head() {
        let a = dump(80, &BASE, &[]);
        let b = dump(100, &BASE, &[]);
        assert!(diff(a.path(), b.path(), Some(90)).is_err());
        assert_eq!(diff(a.path(), b.path(), Some(30)).unwrap().block, 30);
    }
}
