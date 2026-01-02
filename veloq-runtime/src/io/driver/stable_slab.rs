use std::ops::{Index, IndexMut};

const PAGE_SHIFT: usize = 10;
const PAGE_SIZE: usize = 1 << PAGE_SHIFT; // 1024
const PAGE_MASK: usize = PAGE_SIZE - 1;

enum Slot<T> {
    Occupied(T),
    Vacant(usize),
}

pub struct StableSlab<T> {
    // We use Vec<Slot<T>> as a page.
    // Once created and filled, the heap buffer of this Vec behaves like a fixed Box<[Slot<T>]>.
    // We strictly never push/pop from these Vecs after initialization to guarantee pointer stability.
    pages: Vec<Vec<Slot<T>>>,
    free_head: usize,
    len: usize,
}

impl<T> StableSlab<T> {
    pub fn new() -> Self {
        Self {
            pages: Vec::new(),
            free_head: usize::MAX,
            len: 0,
        }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let mut slab = Self::new();
        slab.reserve(capacity);
        slab
    }

    pub fn insert(&mut self, val: T) -> usize {
        let key = if self.free_head != usize::MAX {
            self.free_head
        } else {
            self.add_page();
            self.free_head
        };

        let (page_idx, slot_idx) = Self::unpack_key(key);
        let slot = &mut self.pages[page_idx][slot_idx];

        match slot {
            Slot::Vacant(next) => {
                self.free_head = *next;
                *slot = Slot::Occupied(val);
                self.len += 1;
                key
            }
            Slot::Occupied(_) => unreachable!("Corrupted free list"),
        }
    }

    pub fn remove(&mut self, key: usize) -> T {
        let (page_idx, slot_idx) = Self::unpack_key(key);

        // Let it panic if invalid access
        let slot = &mut self.pages[page_idx][slot_idx];

        // We need to replace Occupied with Vacant using mem::replace equivalent
        // Since we want T out, we can temporarily put Vacant(dummy) then fix it?
        // Or specific swap.

        let new_slot = Slot::Vacant(self.free_head);
        let old_slot = std::mem::replace(slot, new_slot);

        match old_slot {
            Slot::Occupied(val) => {
                self.free_head = key;
                self.len -= 1;
                val
            }
            Slot::Vacant(_) => panic!("Removing already vacant slot"),
        }
    }

    pub fn get(&self, key: usize) -> Option<&T> {
        let (page_idx, slot_idx) = Self::unpack_key(key);
        if let Some(page) = self.pages.get(page_idx) {
            if let Some(slot) = page.get(slot_idx) {
                if let Slot::Occupied(val) = slot {
                    return Some(val);
                }
            }
        }
        None
    }

    pub fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        let (page_idx, slot_idx) = Self::unpack_key(key);
        if let Some(page) = self.pages.get_mut(page_idx) {
            if let Some(slot) = page.get_mut(slot_idx) {
                if let Slot::Occupied(val) = slot {
                    return Some(val);
                }
            }
        }
        None
    }

    pub fn contains(&self, key: usize) -> bool {
        self.get(key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn reserve(&mut self, additional: usize) {
        let available = (self.pages.len() * PAGE_SIZE) - self.len;
        if additional > available {
            let needed = additional - available;
            let pages_needed = (needed + PAGE_SIZE - 1) / PAGE_SIZE;
            for _ in 0..pages_needed {
                self.add_page();
            }
        }
    }

    fn add_page(&mut self) {
        let page_idx = self.pages.len();
        let start_idx = page_idx * PAGE_SIZE;
        let mut page = Vec::with_capacity(PAGE_SIZE);

        let old_head = self.free_head;
        for i in 0..PAGE_SIZE {
            let next = if i == PAGE_SIZE - 1 {
                old_head
            } else {
                start_idx + i + 1
            };
            page.push(Slot::Vacant(next));
        }

        self.pages.push(page);
        self.free_head = start_idx;
    }

    #[inline(always)]
    fn unpack_key(key: usize) -> (usize, usize) {
        (key >> PAGE_SHIFT, key & PAGE_MASK)
    }
}

impl<T> Index<usize> for StableSlab<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("invalid key")
    }
}

impl<T> IndexMut<usize> for StableSlab<T> {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        self.get_mut(index).expect("invalid key")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_crud() {
        let mut slab = StableSlab::new();
        let k1 = slab.insert(10);
        let k2 = slab.insert(20);

        assert_eq!(slab[k1], 10);
        assert_eq!(slab[k2], 20);
        assert_eq!(slab.len(), 2);

        let v = slab.remove(k1);
        assert_eq!(v, 10);
        assert!(slab.get(k1).is_none());
        assert_eq!(slab.len(), 1);

        let k3 = slab.insert(30);
        assert_eq!(slab[k3], 30);
        // k3 should reuse k1's slot usually
        assert_eq!(k3, k1);
    }

    #[test]
    fn test_growth_and_stability() {
        let mut slab = StableSlab::new();
        let mut keys = Vec::new();

        // Fill first page
        for i in 0..PAGE_SIZE {
            keys.push(slab.insert(i));
        }

        // Capture address of first element
        let ptr1 = &slab[keys[0]] as *const _ as usize;

        // Add one more to trigger new page
        let k_new = slab.insert(9999);
        keys.push(k_new);

        // Check address of first element again
        let ptr2 = &slab[keys[0]] as *const _ as usize;
        assert_eq!(ptr1, ptr2, "Address must remain stable after growth");

        // Verify Content
        for i in 0..PAGE_SIZE {
            assert_eq!(slab[keys[i]], i);
        }
        assert_eq!(slab[k_new], 9999);
    }

    #[test]
    fn test_sparse_removal() {
        let mut slab = StableSlab::new();
        let mut keys = Vec::new();
        for i in 0..2000 {
            keys.push(slab.insert(i));
        }

        // Remove every even index
        for i in (0..2000).step_by(2) {
            slab.remove(keys[i]);
        }

        assert_eq!(slab.len(), 1000);

        // Check odds exist
        for i in (1..2000).step_by(2) {
            assert_eq!(slab[keys[i]], i);
        }

        // Re-insert
        for i in 0..500 {
            slab.insert(10000 + i);
        }

        assert_eq!(slab.len(), 1500);
    }
}
