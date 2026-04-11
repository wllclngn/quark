// Slab allocators for fixed-size object pools
//
// Two concrete types:
//   HeapSlab<T> -- growable Vec backing, metadata inline with values
//   MmapSlab   -- fixed-capacity caller-owned region, metadata on heap
//
// Both provide O(1) insert (LIFO free list pop or bump), O(1) remove
// (free list push), O(1) indexed access. Generation counters prevent ABA.

const FREE_END: u32 = u32::MAX;

enum HeapSlot<T> {
    Occupied { generation: u32, value: T },
    Vacant { generation: u32, next_free: u32 },
}

pub struct HeapSlab<T> {
    slots: Vec<HeapSlot<T>>,
    free_head: u32,
    len: u32,
    bump: u32,
}

impl<T> HeapSlab<T> {
    pub fn with_capacity(cap: usize) -> Self {
        Self { slots: Vec::with_capacity(cap), free_head: FREE_END, len: 0, bump: 0 }
    }

    // Reserve index 0 so it's never allocated. Used by HandleTable
    // where handle 0 (index 0 << 2) is invalid in Wine's protocol.
    pub fn skip_index_zero(&mut self) {
        if self.bump == 0 {
            self.slots.push(HeapSlot::Vacant { generation: 0, next_free: FREE_END });
            self.bump = 1;
        }
    }

    // Bump-only insert: never reuses freed slots. Used by HandleTable where
    // Wine caches handle-to-fd mappings and reusing a slot would cause the
    // cache to map to the wrong object.
    pub fn insert_bump(&mut self, value: T) -> (u32, u32) {
        let idx = self.bump;
        self.bump += 1;
        self.slots.push(HeapSlot::Occupied { generation: 0, value });
        self.len += 1;
        (idx, 0)
    }

    pub fn insert_at(&mut self, index: u32, value: T) -> u32 {
        let idx = index as usize;
        while self.slots.len() <= idx {
            let i = self.slots.len() as u32;
            self.slots.push(HeapSlot::Vacant { generation: 0, next_free: FREE_END });
            if i >= self.bump { self.bump = i + 1; }
        }
        if matches!(&self.slots[idx], HeapSlot::Occupied { .. }) {
            self.remove_unchecked(index);
        }
        // Unlink from free list if present
        if self.free_head == index {
            if let HeapSlot::Vacant { next_free, .. } = &self.slots[idx] {
                self.free_head = *next_free;
            }
        } else {
            let mut prev = self.free_head;
            while prev != FREE_END {
                if let HeapSlot::Vacant { next_free, .. } = &self.slots[prev as usize] {
                    if *next_free == index {
                        let target_next = match &self.slots[idx] {
                            HeapSlot::Vacant { next_free, .. } => *next_free,
                            _ => FREE_END,
                        };
                        if let HeapSlot::Vacant { next_free, .. } = &mut self.slots[prev as usize] {
                            *next_free = target_next;
                        }
                        break;
                    }
                    prev = *next_free;
                } else {
                    break;
                }
            }
        }
        let g = match &self.slots[idx] {
            HeapSlot::Vacant { generation, .. } => *generation,
            HeapSlot::Occupied { generation, .. } => *generation,
        };
        self.slots[idx] = HeapSlot::Occupied { generation: g, value };
        self.len += 1;
        if index >= self.bump { self.bump = index + 1; }
        g
    }

    pub fn remove_unchecked(&mut self, index: u32) -> Option<T> {
        if (index as usize) >= self.slots.len() { return None; }
        match &self.slots[index as usize] {
            HeapSlot::Occupied { .. } => {}
            HeapSlot::Vacant { .. } => return None,
        }
        self.remove_inner(index)
    }

    fn remove_inner(&mut self, index: u32) -> Option<T> {
        let old = std::mem::replace(
            &mut self.slots[index as usize],
            HeapSlot::Vacant { generation: 0, next_free: self.free_head },
        );
        match old {
            HeapSlot::Occupied { generation, value } => {
                if let HeapSlot::Vacant { generation: g, .. } = &mut self.slots[index as usize] {
                    *g = generation.wrapping_add(1);
                }
                self.free_head = index;
                self.len -= 1;
                Some(value)
            }
            _ => unreachable!(),
        }
    }

    #[inline]
    pub fn get_unchecked(&self, index: u32) -> Option<&T> {
        match self.slots.get(index as usize)? {
            HeapSlot::Occupied { value, .. } => Some(value),
            HeapSlot::Vacant { .. } => None,
        }
    }

    #[inline]
    pub fn get_mut_unchecked(&mut self, index: u32) -> Option<&mut T> {
        match self.slots.get_mut(index as usize)? {
            HeapSlot::Occupied { value, .. } => Some(value),
            HeapSlot::Vacant { .. } => None,
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize { self.slots.len() }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|slot| match slot {
            HeapSlot::Occupied { value, .. } => Some(value),
            HeapSlot::Vacant { .. } => None,
        })
    }
}


// MmapSlab: data in caller-owned mmap, metadata on heap

struct SlotMeta {
    generation: u32,
    next_free: u32,
}

pub struct MmapSlab {
    meta: Vec<SlotMeta>,
    free_head: u32,
    len: u32,
    bump: u32,
    cap: u32,
}

impl MmapSlab {
    pub fn new(cap: u32) -> Self {
        Self {
            meta: Vec::with_capacity(cap as usize),
            free_head: FREE_END,
            len: 0,
            bump: 0,
            cap,
        }
    }

    pub fn insert(&mut self) -> Option<u32> {
        if self.free_head != FREE_END {
            let idx = self.free_head;
            let m = &mut self.meta[idx as usize];
            self.free_head = m.next_free;
            m.next_free = FREE_END;
            self.len += 1;
            Some(idx)
        } else if self.bump < self.cap {
            let idx = self.bump;
            self.bump += 1;
            self.meta.push(SlotMeta { generation: 0, next_free: FREE_END });
            self.len += 1;
            Some(idx)
        } else {
            None
        }
    }

    pub fn remove(&mut self, index: u32) -> bool {
        if (index as usize) >= self.meta.len() { return false; }
        let m = &mut self.meta[index as usize];
        if m.next_free != FREE_END { return false; }
        m.generation = m.generation.wrapping_add(1);
        m.next_free = self.free_head;
        self.free_head = index;
        self.len -= 1;
        true
    }

    #[inline]
    pub fn high_water(&self) -> u32 { self.bump }
}
