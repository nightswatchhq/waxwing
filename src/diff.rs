use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, bail};
use arrow::array::{Array, ArrayRef, AsArray, PrimitiveArray, RecordBatch};
use arrow::datatypes::{DataType, Int32Type, Int64Type};
use arrow::row::{RowConverter, SortField};
use arrow::util::display::{ArrayFormatter, FormatOptions};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use sha2::{Digest, Sha256};

use super::sort::{self, Sorter};
use super::{DumpTable, check_relative, read_metadata};

const SAMPLES: usize = 3;

/// Every column but `vid` and the upper block bound. `vid` is a storage
/// detail, and the upper bound is the one thing a version changes later.
type Key = [u8; 32];
type Shape = Vec<(String, DataType)>;

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

pub(super) fn batches(path: &Path) -> Result<impl Iterator<Item = Result<RecordBatch>> + use<>> {
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

pub(super) fn vids<'a>(
    batch: &'a RecordBatch,
    file: &str,
) -> Result<&'a PrimitiveArray<Int64Type>> {
    batch
        .column_by_name("vid")
        .and_then(|c| c.as_primitive_opt::<Int64Type>())
        .with_context(|| format!("{file} has no Int64 vid"))
}

/// Refuses a `vid` that does not follow the last one. graph-node writes
/// chunks and clamps in `vid` order, and the clamp join depends on it.
#[derive(Default)]
pub(super) struct VidOrder(Option<i64>);

impl VidOrder {
    pub(super) fn check(&mut self, vid: i64, file: &str) -> Result<()> {
        if self.0.is_some_and(|last| vid <= last) {
            bail!("{file} is not in vid order at vid {vid}");
        }
        self.0 = Some(vid);
        Ok(())
    }
}

struct ClampFile {
    file: String,
    batches: Box<dyn Iterator<Item = Result<RecordBatch>>>,
    batch: Option<RecordBatch>,
    index: usize,
    order: VidOrder,
}

impl ClampFile {
    fn peek(&mut self) -> Result<Option<(i64, i32)>> {
        loop {
            if let Some(batch) = &self.batch
                && self.index < batch.num_rows()
            {
                let vid = vids(batch, &self.file)?.value(self.index);
                return Ok(Some((
                    vid,
                    int32(batch, "block_range_end")?.value(self.index),
                )));
            }
            self.index = 0;
            self.batch = self.batches.next().transpose()?;
            if self.batch.is_none() {
                return Ok(None);
            }
        }
    }

    fn advance(&mut self, vid: i64) -> Result<()> {
        self.order.check(vid, &self.file)?;
        self.index += 1;
        Ok(())
    }
}

/// Upper block bounds recorded by clamp files: versions closed after the
/// chunk holding them was written. Read alongside the chunks, in `vid`
/// order, so only a batch per clamp file is held.
pub(super) struct Clamps(Vec<ClampFile>);

impl Clamps {
    pub(super) fn open(dir: &Path, table: &DumpTable) -> Result<Self> {
        let mut files = Vec::new();
        for clamp in &table.clamps {
            check_relative(&clamp.file)?;
            files.push(ClampFile {
                file: clamp.file.clone(),
                batches: Box::new(batches(&dir.join(&clamp.file))?),
                batch: None,
                index: 0,
                order: VidOrder::default(),
            });
        }
        Ok(Clamps(files))
    }

    /// The clamp for `vid`, if any. Must be asked in ascending `vid` order.
    pub(super) fn end(&mut self, vid: i64) -> Result<Option<i32>> {
        let mut end = None;
        // A later clamp file, from a later dump, overrides an earlier one.
        for file in &mut self.0 {
            while let Some((clamped, at)) = file.peek()?
                && clamped <= vid
            {
                file.advance(clamped)?;
                if clamped == vid {
                    end = Some(at);
                }
            }
        }
        Ok(end)
    }
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
    let mut clamps = Clamps::open(dir, table)?;
    let mut order = VidOrder::default();

    for chunk in &table.chunks {
        check_relative(&chunk.file)?;
        for batch in batches(&dir.join(&chunk.file))? {
            let batch = batch?;
            let schema = batch.schema();
            let vids = vids(&batch, &chunk.file)?;
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
                let vid = vids.value(index);
                order.check(vid, &chunk.file)?;
                let clamp = clamps.end(vid)?;
                let start = starts.value(index);
                if start > cut {
                    continue;
                }
                let end = clamp
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

/// A version as sorted on disk: key, start, end. Byte order is the order of
/// (key, end), with an open version before any closed one.
const RECORD: usize = 44;

fn record(row: &Row) -> [u8; RECORD] {
    let mut out = [0; RECORD];
    out[..32].copy_from_slice(&row.key);
    out[32..36].copy_from_slice(&row.start.to_be_bytes());
    let end = row
        .end
        .map_or(0, |end| i64::from(end) - i64::from(i32::MIN) + 1);
    out[36..].copy_from_slice(&end.to_be_bytes());
    out
}

struct Version {
    key: Key,
    start: i32,
    /// More than one only if the dump holds the same version twice.
    ends: Vec<Option<i32>>,
}

/// A table's versions as of `cut`, in key order, from a sort that spills to
/// disk past `memory` bytes.
fn load(
    dir: &Path,
    table: &DumpTable,
    cut: i32,
    shape: &mut Option<Shape>,
    memory: usize,
) -> Result<impl Iterator<Item = Result<Version>> + use<>> {
    let mut sorter = Sorter::<RECORD>::new(memory);
    visit(dir, table, cut, shape, |row| sorter.push(record(&row)))?;
    let mut records = sorter.finish()?.peekable();

    Ok(std::iter::from_fn(move || {
        let first = match records.next()? {
            Ok(first) => first,
            Err(e) => return Some(Err(e)),
        };
        let decode = |r: &[u8; RECORD]| {
            let end = i64::from_be_bytes(r[36..].try_into().unwrap());
            (end != 0).then(|| (end - 1 + i64::from(i32::MIN)) as i32)
        };
        let mut version = Version {
            key: first[..32].try_into().unwrap(),
            start: i32::from_be_bytes(first[32..36].try_into().unwrap()),
            ends: vec![decode(&first)],
        };
        while let Some(Ok(next)) = records.peek()
            && next[..32] == version.key
        {
            version.ends.push(decode(next));
            records.next();
        }
        Some(Ok(version))
    }))
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
    state_in(dir, at, sort::MEMORY)
}

fn state_in(dir: &Path, at: Option<i32>, memory: usize) -> Result<State> {
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
        let mut leaves = Sorter::<32>::new(memory);
        visit(dir, table, block, &mut shape, |row| {
            let mut leaf = Sha256::new();
            leaf.update(row.key);
            match row.end {
                Some(end) => leaf.update(end.to_be_bytes()),
                None => leaf.update(b"open"),
            }
            leaves.push(leaf.finalize().into())
        })
        .with_context(|| format!("reading {name}"))?;

        let mut hasher = Sha256::new();
        for (column, data_type) in shape.iter().flatten() {
            hasher.update(format!("{column}:{data_type}\0"));
        }
        let mut versions = 0;
        for leaf in leaves.finish()? {
            hasher.update(leaf?);
            versions += 1;
        }
        let root = hex::encode(hasher.finalize());
        all.update(format!("\0{name}\0{root}"));
        tables.push(TableState {
            table: name.clone(),
            versions,
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
pub fn diff(a: &Path, b: &Path, at: Option<i32>) -> Result<Diff> {
    diff_in(a, b, at, sort::MEMORY)
}

fn diff_in(a: &Path, b: &Path, at: Option<i32>, memory: usize) -> Result<Diff> {
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
        let mut versions_a = load(a, table_a, block, &mut shape, memory)
            .with_context(|| format!("reading {name} in A"))?;
        let mut versions_b = load(b, table_b, block, &mut shape, memory)
            .with_context(|| format!("reading {name} in B"))?;

        let mut table = TableDiff {
            table: name.clone(),
            versions: [0, 0],
            only: [0, 0],
            closed_differently: 0,
            first_block: None,
            samples: Vec::new(),
        };
        // A few keys from each side that differ at `first_block`.
        let mut first: [Vec<Key>; 2] = Default::default();
        let mut differs = |table: &mut TableDiff, block: i32, key: Key, sides: [bool; 2]| {
            if table.first_block.is_none_or(|first| block < first) {
                table.first_block = Some(block);
                first = Default::default();
            }
            if table.first_block == Some(block) {
                for (keys, side) in first.iter_mut().zip(sides) {
                    if side && keys.len() < SAMPLES {
                        keys.push(key);
                    }
                }
            }
        };

        let (mut next_a, mut next_b) = (
            versions_a.next().transpose()?,
            versions_b.next().transpose()?,
        );
        loop {
            let order = match (&next_a, &next_b) {
                (None, None) => break,
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(va), Some(vb)) => va.key.cmp(&vb.key),
            };
            match order {
                Ordering::Less => {
                    let va = next_a.take().unwrap();
                    table.versions[0] += 1;
                    table.only[0] += 1;
                    differs(&mut table, va.start, va.key, [true, false]);
                }
                Ordering::Greater => {
                    let vb = next_b.take().unwrap();
                    table.versions[1] += 1;
                    table.only[1] += 1;
                    differs(&mut table, vb.start, vb.key, [false, true]);
                }
                Ordering::Equal => {
                    let (va, vb) = (next_a.take().unwrap(), next_b.take().unwrap());
                    table.versions[0] += 1;
                    table.versions[1] += 1;
                    if va.ends != vb.ends {
                        table.closed_differently += 1;
                        let ends = va.ends.iter().chain(&vb.ends);
                        let block = ends.flatten().min().copied().unwrap_or(va.start);
                        differs(&mut table, block, va.key, [true, true]);
                    }
                }
            }
            if next_a.is_none() {
                next_a = versions_a.next().transpose()?;
            }
            if next_b.is_none() {
                next_b = versions_b.next().transpose()?;
            }
        }

        let [keys_a, keys_b] = first.map(HashSet::from_iter);
        table.samples = samples("A", a, table_a, block, &keys_a)?;
        table
            .samples
            .extend(samples("B", b, table_b, block, &keys_b)?);
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
    fn spilling_to_disk_changes_nothing() {
        let a = dump(100, &BASE, &[]);
        let mut rows = BASE.to_vec();
        rows[2].4 = 8;
        rows.push((4, 64, None, "c", 0));
        rows.push((5, 64, None, "c", 0));
        let b = dump(100, &rows, &[(1, 50)]);
        for dir in [&a, &b] {
            assert_eq!(
                state_in(dir.path(), None, 1).unwrap(),
                state(dir.path(), None).unwrap()
            );
        }
        let spilled = diff_in(a.path(), b.path(), None, 1).unwrap();
        assert_eq!(spilled, diff(a.path(), b.path(), None).unwrap());
        assert_eq!(spilled.tables[0].only, [1, 2]);
    }

    #[test]
    fn a_later_clamp_file_overrides_an_earlier_one() {
        let mut open = BASE;
        open[0].2 = None;
        let a = dump(100, &open, &[(1, 40)]);
        write(
            &a.path().join("Token/clamp_000001.parquet"),
            Schema::new(vec![
                Field::new("vid", DataType::Int64, false),
                Field::new("block_range_end", DataType::Int32, false),
            ]),
            vec![
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(Int32Array::from(vec![50])),
            ],
        );
        let path = a.path().join("metadata.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        metadata["tables"]["Token"]["clamps"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "file": "Token/clamp_000001.parquet" }));
        fs::write(&path, metadata.to_string()).unwrap();

        let b = dump(100, &BASE, &[]);
        assert_eq!(
            state(a.path(), None).unwrap(),
            state(b.path(), None).unwrap()
        );
    }

    #[test]
    fn a_chunk_out_of_vid_order_is_refused() {
        let rows = [(2, 10, None, "a", 1), (1, 20, None, "b", 7)];
        let err = state(dump(100, &rows, &[]).path(), None).unwrap_err();
        assert!(format!("{err:#}").contains("not in vid order"), "{err:#}");
    }

    #[test]
    fn at_must_not_pass_either_head() {
        let a = dump(80, &BASE, &[]);
        let b = dump(100, &BASE, &[]);
        assert!(diff(a.path(), b.path(), Some(90)).is_err());
        assert_eq!(diff(a.path(), b.path(), Some(30)).unwrap().block, 30);
    }
}
