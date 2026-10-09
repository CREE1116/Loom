//! Bounded native LMX expert IO into caller-owned slots (CPU or mapped device memory).
//! This is a paging primitive, not yet connected to the Metal execution graph.
use super::{
    lmx_native::{BlockKind, ExpertPlan},
    lmx_native_file::NativeFile,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

struct Slot<B> {
    buffer: B,
    key: Option<(u32, u32)>,
    bytes: usize,
    used: u64,
}

pub struct ExpertCache<B> {
    file: Arc<NativeFile>,
    blocks: BTreeMap<(u32, u32), String>,
    slots: Vec<Slot<B>>,
    clock: u64,
    hits: u64,
    misses: u64,
    read_bytes: u64,
}

/// Borrowing pins the entire selected set. A backend must retain this lease until its
/// consuming GPU dispatch completes; reusing a slot while GPU work is pending is invalid.
pub struct ExpertBatch<'a, B> {
    cache: &'a ExpertCache<B>,
    mapping: Vec<(u32, usize)>,
}
impl<B: AsRef<[u8]> + AsMut<[u8]>> ExpertCache<B> {
    pub fn new(file: Arc<NativeFile>, plan: &ExpertPlan, mut buffers: Vec<B>) -> Result<Self> {
        plan.validate_for(&file.manifest)?;
        ensure!(
            buffers.len() == plan.slots as usize && !buffers.is_empty(),
            "wrong expert slot count"
        );
        let required =
            usize::try_from(plan.slot_bytes).context("expert slot exceeds address space")?;
        ensure!(
            buffers.iter().all(|b| b.as_ref().len() == required),
            "wrong expert slot capacity"
        );
        ensure!(
            buffers.iter_mut().all(|b| b.as_mut().len() == required),
            "wrong expert slot writable capacity"
        );
        let blocks = file
            .manifest
            .blocks
            .iter()
            .filter_map(|(id, b)| match b.kind {
                BlockKind::Expert { layer, expert } => Some(((layer, expert), id.clone())),
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        ensure!(
            !blocks.is_empty()
                && blocks
                    .values()
                    .all(|id| file.manifest.blocks[id].bytes <= plan.slot_bytes),
            "plan does not fit expert blocks"
        );
        Ok(Self {
            file,
            blocks,
            slots: buffers
                .into_iter()
                .map(|buffer| Slot {
                    buffer,
                    key: None,
                    bytes: 0,
                    used: 0,
                })
                .collect(),
            clock: 0,
            hits: 0,
            misses: 0,
            read_bytes: 0,
        })
    }

    pub fn acquire(&mut self, layer: u32, experts: &[u32]) -> Result<ExpertBatch<'_, B>> {
        let requested: BTreeSet<_> = experts.iter().map(|&e| (layer, e)).collect();
        ensure!(
            !requested.is_empty() && requested.len() <= self.slots.len(),
            "selected expert union exceeds slot budget; split the prefill batch"
        );
        ensure!(
            requested.iter().all(|key| self.blocks.contains_key(key)),
            "unknown layer/expert"
        );
        self.clock = self
            .clock
            .checked_add(1)
            .context("expert cache clock overflow")?;
        let mut mapping = Vec::with_capacity(requested.len());
        for key in &requested {
            let existing = self.slots.iter().position(|s| s.key == Some(*key));
            let index = if let Some(index) = existing {
                self.hits += 1;
                index
            } else {
                // Pin every requested hit BEFORE loading any miss, regardless of ID order.
                let index = self
                    .slots
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.key.is_none_or(|k| !requested.contains(&k)))
                    .min_by_key(|(_, s)| (s.key.is_some(), s.used))
                    .map(|(i, _)| i)
                    .context("no unpinned expert slot")?;
                let id = &self.blocks[key];
                let bytes = usize::try_from(self.file.manifest.blocks[id].bytes)
                    .context("expert exceeds address space")?;
                let slot = &mut self.slots[index];
                slot.key = None;
                slot.bytes = 0;
                let out = slot
                    .buffer
                    .as_mut()
                    .get_mut(..bytes)
                    .context("expert slot writable view is too small")?;
                self.file.read_block_into(id, out)?;
                // A failed/partial read is never published as a valid expert.
                slot.key = Some(*key);
                slot.bytes = bytes;
                self.misses += 1;
                self.read_bytes += bytes as u64;
                index
            };
            self.slots[index].used = self.clock;
            mapping.push((key.1, index));
        }
        Ok(ExpertBatch {
            cache: self,
            mapping,
        })
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        (self.hits, self.misses, self.read_bytes)
    }
}
impl<B: AsRef<[u8]>> ExpertBatch<'_, B> {
    /// Global expert ID -> compact slot index; no change to router probabilities.
    pub fn mapping(&self) -> &[(u32, usize)] {
        &self.mapping
    }
    pub fn bytes(&self, expert: u32) -> Option<&[u8]> {
        self.mapping
            .iter()
            .find(|(id, _)| *id == expert)
            .map(|(_, i)| {
                let slot = &self.cache.slots[*i];
                &slot.buffer.as_ref()[..slot.bytes]
            })
    }
}
