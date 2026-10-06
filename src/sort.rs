//! Sorting more fixed-size records than fit in memory: sorted runs spilled
//! to anonymous files under `TMPDIR`, then merged.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};

use anyhow::Result;

/// Bytes of records held before a run is spilled.
pub(crate) const MEMORY: usize = 256 << 20;
/// Runs merged at once. More are merged in passes, to stay well under the
/// open-file limit (256 by default on macOS).
const FAN_IN: usize = 64;

pub(crate) struct Sorter<const N: usize> {
    buffer: Vec<[u8; N]>,
    limit: usize,
    fan_in: usize,
    runs: Vec<File>,
}

impl<const N: usize> Sorter<N> {
    pub(crate) fn new(memory: usize) -> Self {
        Sorter {
            buffer: Vec::new(),
            limit: (memory / N).max(1),
            fan_in: FAN_IN,
            runs: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, record: [u8; N]) -> Result<()> {
        if self.buffer.len() == self.buffer.capacity() {
            let grow = self.buffer.capacity().max(1024);
            self.buffer
                .reserve_exact(grow.min(self.limit - self.buffer.len()).max(1));
        }
        self.buffer.push(record);
        if self.buffer.len() >= self.limit {
            self.buffer.sort_unstable();
            let run = write_run(self.buffer.drain(..).map(Ok))?;
            self.runs.push(run);
        }
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<Sorted<N>> {
        self.buffer.sort_unstable();
        if self.runs.is_empty() {
            return Ok(Sorted::Memory(self.buffer.into_iter()));
        }
        if !self.buffer.is_empty() {
            let run = write_run(self.buffer.drain(..).map(Ok))?;
            self.runs.push(run);
        }
        drop(self.buffer);

        let mut runs = self.runs;
        while runs.len() > self.fan_in {
            let mut merged = Vec::new();
            let mut rest = runs.into_iter();
            loop {
                let group: Vec<File> = rest.by_ref().take(self.fan_in).collect();
                if group.is_empty() {
                    break;
                }
                merged.push(write_run(Merge::<N>::new(group)?)?);
            }
            runs = merged;
        }
        Ok(Sorted::Merge(Merge::new(runs)?))
    }
}

fn write_run<const N: usize>(records: impl Iterator<Item = Result<[u8; N]>>) -> Result<File> {
    let mut writer = BufWriter::new(tempfile::tempfile()?);
    for record in records {
        writer.write_all(&record?)?;
    }
    let mut file = writer.into_inner().map_err(|e| e.into_error())?;
    file.rewind()?;
    Ok(file)
}

pub(crate) struct Merge<const N: usize> {
    runs: Vec<BufReader<File>>,
    heap: BinaryHeap<Reverse<([u8; N], usize)>>,
}

impl<const N: usize> Merge<N> {
    fn new(files: Vec<File>) -> Result<Self> {
        let mut merge = Merge {
            runs: files.into_iter().map(BufReader::new).collect(),
            heap: BinaryHeap::new(),
        };
        for run in 0..merge.runs.len() {
            merge.refill(run)?;
        }
        Ok(merge)
    }

    fn refill(&mut self, run: usize) -> Result<()> {
        let reader = &mut self.runs[run];
        if reader.fill_buf()?.is_empty() {
            return Ok(());
        }
        let mut record = [0; N];
        reader.read_exact(&mut record)?;
        self.heap.push(Reverse((record, run)));
        Ok(())
    }
}

impl<const N: usize> Iterator for Merge<N> {
    type Item = Result<[u8; N]>;

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((record, run)) = self.heap.pop()?;
        Some(self.refill(run).map(|()| record))
    }
}

pub(crate) enum Sorted<const N: usize> {
    Memory(std::vec::IntoIter<[u8; N]>),
    Merge(Merge<N>),
}

impl<const N: usize> Iterator for Sorted<N> {
    type Item = Result<[u8; N]>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Sorted::Memory(records) => records.next().map(Ok),
            Sorted::Merge(merge) => merge.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sort(records: &[[u8; 2]], memory: usize, fan_in: usize) -> Vec<[u8; 2]> {
        let mut sorter = Sorter::<2>::new(memory);
        sorter.fan_in = fan_in;
        for record in records {
            sorter.push(*record).unwrap();
        }
        sorter.finish().unwrap().map(Result::unwrap).collect()
    }

    #[test]
    fn spilled_runs_merge_to_the_in_memory_order() {
        let records: Vec<[u8; 2]> = (0..1000u32)
            .map(|i| ((i * 7919) % 613) as u16)
            .map(u16::to_be_bytes)
            .collect();
        let mut expected = records.clone();
        expected.sort();
        assert_eq!(sort(&records, MEMORY, FAN_IN), expected);
        // 1000 runs of one record, and 334 of three, merged in several passes.
        assert_eq!(sort(&records, 2, 2), expected);
        assert_eq!(sort(&records, 6, 3), expected);
        assert_eq!(sort(&records, 2000, 64), expected);
        assert!(sort(&[], 2, 2).is_empty());
    }
}
