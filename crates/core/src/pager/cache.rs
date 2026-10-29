//! The buffer cache: a fixed number of in-memory frames, each holding one
//! page, with least-recently-used (LRU) eviction.

use std::collections::HashMap;

use super::{Page, PageId};

/// One cached page.
pub struct Frame {
    pub page: PageId,
    pub data: Box<Page>,
    /// Changed since it was read or last written to storage.
    pub dirty: bool,
}

/// Marks "no slot" in the LRU links.
const NIL: usize = usize::MAX;

/// Frames live in fixed slots. The LRU order is a doubly linked list threaded
/// through the slots by index (`prev`/`next`), so moving a page to the front
/// or dropping the oldest one is O(1) with no allocation.
pub struct Cache {
    slots: Vec<Option<Frame>>,
    /// `prev[s]` is the slot used just more recently than `s`; `next[s]` just less.
    prev: Vec<usize>,
    next: Vec<usize>,
    /// Most recently used slot.
    head: usize,
    /// Least recently used slot: the next one to evict.
    tail: usize,
    /// Empty slots ready for `insert`.
    free_slots: Vec<usize>,
    /// Which slot holds each cached page.
    map: HashMap<PageId, usize>,
}

impl Cache {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "the cache needs at least one frame");
        Cache {
            slots: (0..capacity).map(|_| None).collect(),
            prev: vec![NIL; capacity],
            next: vec![NIL; capacity],
            head: NIL,
            tail: NIL,
            // Reversed so slot 0 is handed out first.
            free_slots: (0..capacity).rev().collect(),
            map: HashMap::with_capacity(capacity),
        }
    }

    pub fn contains(&self, page: PageId) -> bool {
        self.map.contains_key(&page)
    }

    /// Look up a page and mark it most recently used.
    pub fn get(&mut self, page: PageId) -> Option<&mut Frame> {
        let slot = *self.map.get(&page)?;
        self.unlink(slot);
        self.push_front(slot);
        self.slots[slot].as_mut()
    }

    /// Look up a page without changing the LRU order.
    pub fn peek_mut(&mut self, page: PageId) -> Option<&mut Frame> {
        let slot = *self.map.get(&page)?;
        self.slots[slot].as_mut()
    }

    /// If every frame is in use, remove and return the least recently used one.
    pub fn evict_if_full(&mut self) -> Option<Frame> {
        if !self.free_slots.is_empty() {
            return None;
        }
        let slot = self.tail;
        self.unlink(slot);
        let frame = self.slots[slot]
            .take()
            .expect("a linked slot holds a frame");
        self.map.remove(&frame.page);
        self.free_slots.push(slot);
        Some(frame)
    }

    /// Add a page as the most recently used. There must be a free frame.
    pub fn insert(&mut self, frame: Frame) {
        assert!(
            !self.contains(frame.page),
            "page {} already cached",
            frame.page
        );
        let slot = self
            .free_slots
            .pop()
            .expect("cache is full: call evict_if_full first");
        self.map.insert(frame.page, slot);
        self.slots[slot] = Some(frame);
        self.push_front(slot);
    }

    /// Ids of all dirty pages, in ascending order.
    pub fn dirty_pages(&self) -> Vec<PageId> {
        let mut pages: Vec<PageId> = self
            .slots
            .iter()
            .flatten()
            .filter(|frame| frame.dirty)
            .map(|frame| frame.page)
            .collect();
        pages.sort_unstable();
        pages
    }

    /// Cached page ids from most to least recently used.
    pub fn lru_order(&self) -> Vec<PageId> {
        let mut order = Vec::with_capacity(self.map.len());
        let mut slot = self.head;
        while slot != NIL {
            order.push(self.slots[slot].as_ref().unwrap().page);
            slot = self.next[slot];
        }
        order
    }

    /// Take `slot` out of the LRU list, joining its neighbours together.
    fn unlink(&mut self, slot: usize) {
        let (prev, next) = (self.prev[slot], self.next[slot]);
        if prev == NIL {
            self.head = next;
        } else {
            self.next[prev] = next;
        }
        if next == NIL {
            self.tail = prev;
        } else {
            self.prev[next] = prev;
        }
        self.prev[slot] = NIL;
        self.next[slot] = NIL;
    }

    /// Put `slot` at the most-recently-used end of the list.
    fn push_front(&mut self, slot: usize) {
        self.prev[slot] = NIL;
        self.next[slot] = self.head;
        if self.head == NIL {
            self.tail = slot;
        } else {
            self.prev[self.head] = slot;
        }
        self.head = slot;
    }
}

#[cfg(test)]
mod tests {
    use super::super::PAGE_SIZE;
    use super::*;

    fn frame(page: PageId, dirty: bool) -> Frame {
        Frame {
            page,
            data: Box::new([0; PAGE_SIZE]),
            dirty,
        }
    }

    #[test]
    fn get_moves_page_to_front() {
        let mut cache = Cache::new(3);
        for page in 1..=3 {
            cache.insert(frame(page, false));
        }
        assert_eq!(cache.lru_order(), vec![3, 2, 1]);
        assert!(cache.get(1).is_some());
        assert_eq!(cache.lru_order(), vec![1, 3, 2]);
        assert!(cache.get(9).is_none());
    }

    #[test]
    fn evicts_least_recently_used_only_when_full() {
        let mut cache = Cache::new(2);
        cache.insert(frame(1, false));
        assert!(cache.evict_if_full().is_none());
        cache.insert(frame(2, true));
        cache.get(1);

        let victim = cache.evict_if_full().expect("cache is full");
        assert_eq!(victim.page, 2);
        assert!(victim.dirty);
        assert!(!cache.contains(2));

        cache.insert(frame(3, false));
        assert_eq!(cache.lru_order(), vec![3, 1]);
    }

    #[test]
    fn peek_does_not_change_order() {
        let mut cache = Cache::new(2);
        cache.insert(frame(1, false));
        cache.insert(frame(2, false));
        cache.peek_mut(1).unwrap().dirty = true;
        assert_eq!(cache.lru_order(), vec![2, 1]);
    }

    #[test]
    fn dirty_pages_are_sorted() {
        let mut cache = Cache::new(4);
        cache.insert(frame(9, true));
        cache.insert(frame(5, false));
        cache.insert(frame(2, true));
        assert_eq!(cache.dirty_pages(), vec![2, 9]);
    }

    #[test]
    fn single_frame_cache_works() {
        let mut cache = Cache::new(1);
        cache.insert(frame(1, false));
        assert_eq!(cache.evict_if_full().unwrap().page, 1);
        cache.insert(frame(2, false));
        assert_eq!(cache.lru_order(), vec![2]);
    }
}
