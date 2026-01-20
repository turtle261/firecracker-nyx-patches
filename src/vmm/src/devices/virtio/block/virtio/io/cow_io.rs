// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{ReadVolatile, VolatileSlice};

use crate::devices::virtio::block::virtio::io::sync_io::SyncIoError;
use crate::devices::virtio::block::virtio::SECTOR_SIZE;
use crate::vstate::memory::{GuestAddress, GuestMemory, GuestMemoryMmap};

const SECTOR_SIZE_USIZE: usize = SECTOR_SIZE as usize;
const SECTOR_SIZE_U64: u64 = SECTOR_SIZE as u64;

type SnapshotId = u32;

#[derive(Debug, Clone)]
pub struct CowCache {
    pub id: SnapshotId,
    areas: Vec<[u8; SECTOR_SIZE_USIZE]>,
    disc_offset_to_area_idx: BTreeMap<u64, usize>,
    previous: Option<Weak<CowCache>>,
}

impl Serialize for CowCache {
    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        panic!("cow cache does not support serialization")
    }
}

impl<'de> Deserialize<'de> for CowCache {
    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        panic!("cow cache does not support deserialization")
    }
}

impl CowCache {
    pub fn new(id: SnapshotId, previous: Option<Weak<CowCache>>) -> Self {
        Self {
            id,
            areas: Vec::new(),
            disc_offset_to_area_idx: BTreeMap::new(),
            previous,
        }
    }

    pub fn write<Bitmap: BitmapSlice>(&mut self, offset: u64, src: &VolatileSlice<Bitmap>) {
        assert_eq!(
            offset % SECTOR_SIZE_U64,
            0,
            "write offset is not aligned to SECTOR_SIZE"
        );
        assert_eq!(
            src.len() % SECTOR_SIZE_USIZE,
            0,
            "len of data written is not aligned to SECTOR_SIZE"
        );
        for sector_i in 0..(src.len() as u64) / SECTOR_SIZE_U64 {
            let disc_offset = offset + sector_i * SECTOR_SIZE_U64;
            let area_idx = if let Some(area) = self.disc_offset_to_area_idx.get(&disc_offset) {
                *area
            } else {
                let area_idx = self.areas.len();
                self.disc_offset_to_area_idx.insert(disc_offset, area_idx);
                self.areas.push([0; SECTOR_SIZE_USIZE]);
                area_idx
            };
            src.subslice((sector_i * SECTOR_SIZE_U64) as usize, SECTOR_SIZE_USIZE)
                .unwrap()
                .copy_to(&mut self.areas[area_idx]);
        }
    }

    pub fn try_read<Bitmap: BitmapSlice>(
        &self,
        offset: u64,
        dest: &mut VolatileSlice<Bitmap>,
    ) -> bool {
        if let Some(area_idx) = self.disc_offset_to_area_idx.get(&offset) {
            dest.copy_from(&self.areas[*area_idx]);
            return true;
        }
        if let Some(prev) = self.previous.as_ref() {
            let arc_prev = prev
                .upgrade()
                .expect("cow cache snapshot was dropped unexpectedly");
            return arc_prev.try_read(offset, dest);
        }
        false
    }

    pub fn read<Bitmap: BitmapSlice>(
        &self,
        file: &mut File,
        offset: u64,
        dest: &mut VolatileSlice<Bitmap>,
    ) {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.read_exact_volatile(dest).unwrap();
        let bounds = offset..(offset + dest.len() as u64);
        for disc_offset in bounds.step_by(SECTOR_SIZE_USIZE) {
            let slice_offset = disc_offset - offset;
            let mut sector_slice = dest
                .subslice(slice_offset as usize, SECTOR_SIZE_USIZE)
                .unwrap();
            let _ = self.try_read(disc_offset, &mut sector_slice);
        }
    }

    pub fn reset_to(&mut self, new_id: SnapshotId, snapshot: Option<Weak<CowCache>>) {
        self.id = new_id;
        self.areas.clear();
        self.disc_offset_to_area_idx.clear();
        self.previous = snapshot;
    }
}

#[derive(Debug)]
pub struct CowCacheTree {
    current: CowCache,
    saved: HashMap<SnapshotId, Weak<CowCache>>,
}

impl CowCacheTree {
    pub fn new() -> Self {
        let current = CowCache::new(0, None);
        Self {
            current,
            saved: HashMap::new(),
        }
    }

    pub fn reset_to(&mut self, id: SnapshotId) {
        let new_id = self.saved.len() as SnapshotId;
        self.current
            .reset_to(new_id, Some(Weak::clone(&self.saved[&id])));
    }

    pub fn snapshot(&mut self) -> Arc<CowCache> {
        let cur = std::mem::replace(&mut self.current, CowCache::new(0, None));
        let cur_id = cur.id;
        assert_eq!(cur_id, self.saved.len() as SnapshotId);
        let current_cache = Arc::new(cur);
        let current_cache_weak = Arc::downgrade(&current_cache);
        self.saved.insert(cur_id, Weak::clone(&current_cache_weak));
        let new_id = self.saved.len() as SnapshotId;
        self.current.id = new_id;
        self.current.previous = Some(current_cache_weak);
        self.garbage_collect();
        current_cache
    }

    pub fn garbage_collect(&mut self) {
        let mut garbage_ids = Vec::new();
        for (id, snap) in self.saved.iter() {
            if Weak::upgrade(snap).is_none() {
                garbage_ids.push(*id);
            }
        }
        for id in garbage_ids {
            self.saved.remove(&id);
        }
    }

    pub fn write<Bitmap: BitmapSlice>(&mut self, offset: u64, src: &VolatileSlice<Bitmap>) {
        self.current.write(offset, src);
    }

    pub fn read<Bitmap: BitmapSlice>(
        &self,
        file: &mut File,
        offset: u64,
        dest: &mut VolatileSlice<Bitmap>,
    ) {
        self.current.read(file, offset, dest);
    }
}

#[derive(Debug)]
pub struct CowFileEngine {
    file: File,
    cache: Arc<Mutex<CowCacheTree>>,
}

// SAFETY: `File` is send and ultimately a POD.
unsafe impl Send for CowFileEngine {}

impl CowFileEngine {
    pub fn from_file(file: File) -> CowFileEngine {
        CowFileEngine {
            file,
            cache: Arc::new(Mutex::new(CowCacheTree::new())),
        }
    }

    #[cfg(test)]
    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn update_file(&mut self, file: File) {
        self.file = file
    }

    pub fn read(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        mem.get_slice(addr, count as usize)
            .and_then(|mut slice| {
                self.cache.lock().unwrap().current.read(&mut self.file, offset, &mut slice);
                Ok(())
            })
            .map_err(SyncIoError::Transfer)?;
        Ok(count)
    }

    pub fn write(
        &mut self,
        offset: u64,
        mem: &GuestMemoryMmap,
        addr: GuestAddress,
        count: u32,
    ) -> Result<u32, SyncIoError> {
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(SyncIoError::Seek)?;
        mem.get_slice(addr, count as usize)
            .and_then(|slice| {
                self.cache.lock().unwrap().current.write(offset, &slice);
                Ok(())
            })
            .map_err(SyncIoError::Transfer)?;
        Ok(count)
    }

    pub fn reset_to(&self, id: SnapshotId) {
        self.cache.lock().unwrap().reset_to(id);
    }

    pub fn snapshot(&self) -> Arc<CowCache> {
        self.cache.lock().unwrap().snapshot()
    }

    pub fn flush(&mut self) -> Result<(), SyncIoError> {
        Ok(())
    }
}
