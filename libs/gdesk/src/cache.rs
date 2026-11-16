use crate::{
    math::{tile_hash, TILE_RGB_BYTES},
    Error, Result,
};
use std::collections::{HashMap, VecDeque};

pub const CACHE_CAPACITY: usize = 4096;
/// Pure function of the tile hash, so a sender and a receiver that see the same lossless tiles in
/// any order and with any losses agree on the slot.
pub fn deterministic_slot(hash: u64) -> u16 {
    ((hash ^ (hash >> 16)) as u16) & (CACHE_CAPACITY as u16 - 1)
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub hash: u64,
    pub rgb: Vec<u8>,
}
#[derive(Debug, Clone)]
pub struct DeterministicLruCache {
    slots: Vec<Option<CacheEntry>>,
    hashes: HashMap<u64, Vec<u16>>,
    lru: VecDeque<u16>,
    pub confirmed_slots: Vec<bool>,
}
impl Default for DeterministicLruCache {
    fn default() -> Self {
        Self::new(CACHE_CAPACITY)
    }
}
impl DeterministicLruCache {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.clamp(1, CACHE_CAPACITY);
        Self {
            slots: vec![None; capacity],
            hashes: HashMap::new(),
            lru: VecDeque::new(),
            confirmed_slots: vec![false; capacity],
        }
    }
    pub fn clear(&mut self) {
        self.slots.fill(None);
        self.hashes.clear();
        self.lru.clear();
        self.confirmed_slots.fill(false);
    }
    pub fn len(&self) -> usize {
        self.lru.len()
    }
    pub fn is_empty(&self) -> bool {
        self.lru.is_empty()
    }
    pub fn find(&self, hash: u64, rgb: &[u8], require_confirmed: bool) -> Option<u16> {
        self.hashes.get(&hash)?.iter().copied().find(|slot| {
            (!require_confirmed || self.confirmed_slots[*slot as usize])
                && self.slots[*slot as usize]
                    .as_ref()
                    .is_some_and(|entry| entry.rgb == rgb)
        })
    }
    pub fn get(&self, slot: u16) -> Option<&CacheEntry> {
        self.slots.get(slot as usize)?.as_ref()
    }
    pub fn touch(&mut self, slot: u16) -> Result<()> {
        let position = self
            .lru
            .iter()
            .position(|s| *s == slot)
            .ok_or(Error("missing cache slot"))?;
        self.lru.remove(position);
        self.lru.push_front(slot);
        Ok(())
    }
    pub fn insert(&mut self, rgb: &[u8]) -> Result<u16> {
        if rgb.len() != TILE_RGB_BYTES {
            return Err(Error("invalid cached tile"));
        }
        let hash = tile_hash(rgb);
        if let Some(slot) = self.find(hash, rgb, false) {
            self.touch(slot)?;
            return Ok(slot);
        }
        let slot = if self.lru.len() < self.slots.len() {
            self.lru.len() as u16
        } else {
            self.lru.pop_back().ok_or(Error("empty cache LRU"))?
        };
        if let Some(old) = self.slots[slot as usize].take() {
            if let Some(slots) = self.hashes.get_mut(&old.hash) {
                slots.retain(|s| *s != slot);
                if slots.is_empty() {
                    self.hashes.remove(&old.hash);
                }
            }
        }
        self.slots[slot as usize] = Some(CacheEntry {
            hash,
            rgb: rgb.to_vec(),
        });
        self.hashes.entry(hash).or_default().push(slot);
        self.confirmed_slots[slot as usize] = false;
        self.lru.push_front(slot);
        Ok(slot)
    }
    /// Direct-mapped insert; a slot already holding the same tile keeps its confirmation.
    pub fn insert_at_slot(&mut self, slot: u16, rgb: &[u8]) -> Result<()> {
        if rgb.len() != TILE_RGB_BYTES || slot as usize >= self.slots.len() {
            return Err(Error("invalid cached tile"));
        }
        let hash = tile_hash(rgb);
        if let Some(old) = self.slots[slot as usize].take() {
            if old.hash == hash && old.rgb == rgb {
                self.slots[slot as usize] = Some(old);
                return Ok(());
            }
            if let Some(slots) = self.hashes.get_mut(&old.hash) {
                slots.retain(|s| *s != slot);
                if slots.is_empty() {
                    self.hashes.remove(&old.hash);
                }
            }
        }
        self.slots[slot as usize] = Some(CacheEntry {
            hash,
            rgb: rgb.to_vec(),
        });
        self.hashes.entry(hash).or_default().push(slot);
        self.confirmed_slots[slot as usize] = false;
        if !self.lru.contains(&slot) {
            self.lru.push_front(slot);
        }
        Ok(())
    }
    pub fn confirm_slot(&mut self, slot: u16) -> Result<()> {
        if self.get(slot).is_none() {
            return Err(Error("missing cache slot"));
        }
        self.confirmed_slots[slot as usize] = true;
        Ok(())
    }
    pub fn lru_slots(&self) -> impl Iterator<Item = u16> + '_ {
        self.lru.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deterministic_eviction_confirmation_and_collision_check() {
        let mut cache = DeterministicLruCache::new(2);
        let a = vec![1; TILE_RGB_BYTES];
        let b = vec![2; TILE_RGB_BYTES];
        let c = vec![3; TILE_RGB_BYTES];
        assert_eq!(cache.insert(&a).unwrap(), 0);
        assert_eq!(cache.insert(&b).unwrap(), 1);
        assert_eq!(cache.find(tile_hash(&a), &a, true), None);
        cache.confirm_slot(0).unwrap();
        assert_eq!(cache.find(tile_hash(&a), &a, true), Some(0));
        assert_eq!(cache.find(tile_hash(&a), &b, false), None);
        cache.touch(0).unwrap();
        assert_eq!(cache.insert(&c).unwrap(), 1);
        assert_eq!(cache.find(tile_hash(&b), &b, false), None);
        assert_eq!(cache.lru_slots().collect::<Vec<_>>(), [1, 0]);
        assert!(!cache.confirmed_slots[1]);
    }
}
