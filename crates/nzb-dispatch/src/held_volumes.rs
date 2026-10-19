//! Deferred PAR2 recovery volumes (WI-1230).
//!
//! With `par2.defer_volumes` on, a job's recovery-volume articles are not put
//! on the work queue at submission. They are parked here, one entry per
//! volume, ordered by the volume's first exponent. The job's in-flight slice
//! map (WI-1228) decides how many are needed: when it shows `d` damaged
//! slices, the smallest prefix of volumes whose blocks cover
//! `d + REPAIR_MARGIN` is released to the queue, and more follow as damage
//! grows. A job whose data finishes clean completes without its volumes.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::download_engine::{SharedWorkQueue, WorkItem};

/// Recovery blocks fetched beyond the damage seen, so repair still succeeds
/// when a volume turns out damaged itself (zurg's `repairMargin`).
pub(crate) const REPAIR_MARGIN: usize = 8;

/// One held recovery volume and the work items that fetch it.
struct HeldVolume {
    file_id: String,
    first_exponent: u32,
    blocks: u32,
    items: Vec<WorkItem>,
}

#[derive(Default)]
struct Inner {
    /// Volumes not yet released, lowest first exponent first.
    held: Vec<HeldVolume>,
    /// Recovery blocks already released to the queue.
    released_blocks: usize,
    queue: Option<Arc<SharedWorkQueue>>,
}

/// The held volumes of one job.
#[derive(Default)]
pub(crate) struct HeldVolumes {
    inner: Mutex<Inner>,
}

impl HeldVolumes {
    /// Split `items` into the ones to enqueue now and the volume items to
    /// hold. `volumes` maps a volume's NZB file id to `(first exponent,
    /// blocks)`. Returns the items to enqueue; the rest are held and released
    /// through `queue`.
    pub(crate) fn hold(
        &self,
        items: Vec<WorkItem>,
        volumes: &HashMap<String, (u32, u32)>,
        queue: Arc<SharedWorkQueue>,
    ) -> Vec<WorkItem> {
        let mut by_file: HashMap<&str, HeldVolume> = HashMap::new();
        let mut now = Vec::with_capacity(items.len());
        for item in items {
            match volumes.get_key_value(&item.file_id) {
                Some((file_id, &(first_exponent, blocks))) => by_file
                    .entry(file_id.as_str())
                    .or_insert_with(|| HeldVolume {
                        file_id: file_id.clone(),
                        first_exponent,
                        blocks,
                        items: Vec::new(),
                    })
                    .items
                    .push(item),
                None => now.push(item),
            }
        }
        let mut held: Vec<HeldVolume> = by_file.into_values().collect();
        held.sort_by_key(|v| v.first_exponent);
        let mut inner = self.inner.lock();
        inner.held = held;
        inner.queue = Some(queue);
        now
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.inner.lock().held.is_empty()
    }

    /// Release the smallest prefix of held volumes that brings the released
    /// blocks to at least `need`, or every volume when the held ones cannot
    /// reach it. Returns the queue and the released items.
    pub(crate) fn release_for(&self, need: usize) -> Option<(Arc<SharedWorkQueue>, Vec<WorkItem>)> {
        let mut inner = self.inner.lock();
        let mut items = Vec::new();
        while inner.released_blocks < need && !inner.held.is_empty() {
            let volume = inner.held.remove(0);
            inner.released_blocks += volume.blocks as usize;
            items.extend(volume.items);
        }
        if items.is_empty() {
            return None;
        }
        inner.queue.clone().map(|q| (q, items))
    }

    /// Release every held volume.
    pub(crate) fn release_all(&self) -> Option<(Arc<SharedWorkQueue>, Vec<WorkItem>)> {
        self.release_for(usize::MAX)
    }

    /// NZB file ids of the volumes still held, lowest first exponent first.
    pub(crate) fn held_file_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .held
            .iter()
            .map(|v| v.file_id.clone())
            .collect()
    }

    /// Recovery blocks released plus those still held.
    pub(crate) fn total_blocks(&self) -> usize {
        let inner = self.inner.lock();
        inner.released_blocks + inner.held.iter().map(|v| v.blocks as usize).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(file_id: &str, segment: u32) -> WorkItem {
        WorkItem {
            job_id: "j".into(),
            file_id: file_id.into(),
            filename: format!("{file_id}.par2"),
            message_id: format!("{file_id}-{segment}@test"),
            segment_number: segment,
            tried_servers: Vec::new(),
            provider_outcomes: HashMap::new(),
            tries_on_current: 0,
            outage_tries: 0,
            confirm_rounds: 0,
        }
    }

    fn held() -> (HeldVolumes, Vec<WorkItem>) {
        let volumes: HashMap<String, (u32, u32)> = [
            ("v8".to_string(), (7, 8)),
            ("v1".to_string(), (0, 1)),
            ("v4".to_string(), (3, 4)),
            ("v2".to_string(), (1, 2)),
        ]
        .into();
        let items = vec![
            item("data", 1),
            item("v8", 1),
            item("v1", 1),
            item("v4", 1),
            item("v4", 2),
            item("v2", 1),
        ];
        let h = HeldVolumes::default();
        let now = h.hold(items, &volumes, Arc::new(SharedWorkQueue::new()));
        (h, now)
    }

    fn files(items: &[WorkItem]) -> Vec<&str> {
        let mut f: Vec<&str> = items.iter().map(|i| i.file_id.as_str()).collect();
        f.dedup();
        f
    }

    #[test]
    fn only_data_is_enqueued() {
        let (h, now) = held();
        assert_eq!(files(&now), ["data"]);
        assert_eq!(h.total_blocks(), 15);
        assert_eq!(h.held_file_ids(), ["v1", "v2", "v4", "v8"]);
    }

    #[test]
    fn releases_the_smallest_covering_prefix_then_more() {
        let (h, _) = held();
        let (_, first) = h.release_for(3).unwrap();
        assert_eq!(files(&first), ["v1", "v2"]);
        assert!(h.release_for(3).is_none());
        let (_, more) = h.release_for(4).unwrap();
        assert_eq!(files(&more), ["v4"]);
        assert_eq!(more.len(), 2);
        let (_, rest) = h.release_for(100).unwrap();
        assert_eq!(files(&rest), ["v8"]);
        assert!(h.held_file_ids().is_empty());
        assert!(h.is_empty());
        assert!(h.release_all().is_none());
    }
}
