//! Ordered range scans.
//!
//! A scan walks down the tree once, to the leaf where the range starts, and
//! from then on follows the leaf chain to the right.

use std::ops::Bound;

use super::node::{Entry, Node};
use super::{BTree, child_index, read_node};
use crate::error::{Error, Result};
use crate::pager::{PageId, Pager};

/// An iterator over the entries in a key range, in key order.
/// It borrows the pager, so the tree can't change while it runs.
pub struct Range<'p> {
    pager: &'p mut Pager,
    /// Entries of the current leaf not yet returned.
    entries: std::vec::IntoIter<Entry>,
    /// The leaf after the current one (0 = no more leaves).
    next_leaf: PageId,
    end: Bound<Vec<u8>>,
    done: bool,
}

impl BTree {
    /// Entries with keys between `start` and `end`, in order.
    pub fn range<'p>(
        &self,
        pager: &'p mut Pager,
        start: Bound<&[u8]>,
        end: Bound<&[u8]>,
    ) -> Result<Range<'p>> {
        // Walk down to the leaf where `start` would be.
        let mut page = self.root;
        let (mut entries, next_leaf) = loop {
            match read_node(pager, page)? {
                Node::Internal { keys, children } => {
                    page = match start {
                        Bound::Unbounded => children[0],
                        Bound::Included(k) | Bound::Excluded(k) => children[child_index(&keys, k)],
                    };
                }
                Node::Leaf { entries, next } => break (entries, next),
            }
        };
        // Drop the entries before `start` in that first leaf. Every later leaf
        // starts at or above a separator that is above `start`.
        let skip = match start {
            Bound::Unbounded => 0,
            Bound::Included(s) => entries.partition_point(|(k, _)| k.as_slice() < s),
            Bound::Excluded(s) => entries.partition_point(|(k, _)| k.as_slice() <= s),
        };
        entries.drain(..skip);
        Ok(Range {
            pager,
            entries: entries.into_iter(),
            next_leaf,
            end: end.map(<[u8]>::to_vec),
            done: false,
        })
    }

    /// Every entry, in key order.
    pub fn iter<'p>(&self, pager: &'p mut Pager) -> Result<Range<'p>> {
        self.range(pager, Bound::Unbounded, Bound::Unbounded)
    }
}

impl Iterator for Range<'_> {
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            if let Some((key, value)) = self.entries.next() {
                let in_range = match &self.end {
                    Bound::Unbounded => true,
                    Bound::Included(end) => key <= *end,
                    Bound::Excluded(end) => key < *end,
                };
                if in_range {
                    return Some(Ok((key, value)));
                }
                // Keys only grow from here, so the scan is over.
                self.done = true;
            } else if self.next_leaf == 0 {
                self.done = true;
            } else {
                // This leaf is used up: step right along the leaf chain.
                let page = self.next_leaf;
                match read_node(self.pager, page) {
                    Ok(Node::Leaf { entries, next }) => {
                        self.entries = entries.into_iter();
                        self.next_leaf = next;
                    }
                    Ok(Node::Internal { .. }) => {
                        self.done = true;
                        let msg = format!("leaf chain points to internal page {page}");
                        return Some(Err(Error::Corrupt(msg)));
                    }
                    Err(err) => {
                        self.done = true;
                        return Some(Err(err));
                    }
                }
            }
        }
        None
    }
}
