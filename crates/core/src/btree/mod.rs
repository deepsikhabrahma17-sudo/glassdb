//! B+ tree: ordered byte-string keys mapped to byte-string values, stored in pages.
//!
//! Keys compare byte by byte, like `BTreeMap<Vec<u8>, Vec<u8>>`. Every entry
//! lives in a leaf; internal nodes only hold separator keys that steer a
//! search to the right child. Leaves are linked left to right, so a range
//! scan walks down once and then along the leaves.
//!
//! The tree is just its root page id: pass the shared `Pager` into each call.
//! The root page id never changes (the root grows and shrinks in place), so
//! whoever stores it never has to update it.

mod check;
mod cursor;
mod modify;
pub mod node;
mod rebalance;

#[cfg(test)]
mod tests;

use crate::error::{Error, Result};
use crate::pager::{PageId, Pager};
use crate::trace::Event;
pub use check::TreeStats;
pub use cursor::Range;
use modify::{Change, Outcome, modify};
use node::{Entry, Node};
pub use node::{MAX_ENTRY, MAX_KEY};

pub struct BTree {
    root: PageId,
}

impl BTree {
    /// Make a new, empty tree: a single empty leaf.
    pub fn create(pager: &mut Pager) -> Result<BTree> {
        let root = pager.allocate()?;
        write_node(pager, root, &Node::empty_leaf())?;
        Ok(BTree { root })
    }

    /// Use the existing tree whose root is `root`.
    pub fn open(root: PageId) -> BTree {
        BTree { root }
    }

    pub fn root(&self) -> PageId {
        self.root
    }

    pub fn get(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut page = self.root;
        loop {
            match read_node(pager, page)? {
                Node::Internal { keys, children } => page = children[child_index(&keys, key)],
                Node::Leaf { mut entries, .. } => {
                    let found = entries.binary_search_by(|(k, _)| k.as_slice().cmp(key));
                    return Ok(found.ok().map(|i| entries.swap_remove(i).1));
                }
            }
        }
    }

    /// Insert or replace `key`. Returns the old value if the key existed.
    pub fn insert(&self, pager: &mut Pager, key: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>> {
        if key.len() > MAX_KEY || key.len() + value.len() > MAX_ENTRY {
            return Err(Error::EntryTooLarge {
                key_len: key.len(),
                value_len: value.len(),
            });
        }
        let (old, outcome) = modify(pager, self.root, key, &Change::Insert(value))?;
        self.fix_root(pager, outcome)?;
        Ok(old)
    }

    /// Remove `key`. Returns its value if it existed.
    pub fn delete(&self, pager: &mut Pager, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (old, outcome) = modify(pager, self.root, key, &Change::Delete)?;
        self.fix_root(pager, outcome)?;
        Ok(old)
    }

    /// The entry with the largest key, if any.
    pub fn last(&self, pager: &mut Pager) -> Result<Option<Entry>> {
        let mut page = self.root;
        loop {
            match read_node(pager, page)? {
                Node::Internal { children, .. } => {
                    page = *children.last().expect("internal node has a child")
                }
                // Only the root can be an empty leaf, so this is the true last entry.
                Node::Leaf { mut entries, .. } => return Ok(entries.pop()),
            }
        }
    }

    /// Free every page of the tree, root included. The tree is gone afterwards.
    pub fn destroy(self, pager: &mut Pager) -> Result<()> {
        free_subtree(pager, self.root)
    }

    /// The root has no parent to repair it, so it is handled here, keeping
    /// its page id fixed.
    fn fix_root(&self, pager: &mut Pager, outcome: Outcome) -> Result<()> {
        match outcome {
            Outcome::Fine => {}
            Outcome::Split { separator, right } => {
                // The root page holds the left half. Move that half to a new
                // page and make the root the parent of both halves.
                let left = pager.allocate()?;
                let left_half = read_node(pager, self.root)?;
                write_node(pager, left, &left_half)?;
                let root = Node::Internal {
                    keys: vec![separator],
                    children: vec![left, right],
                };
                write_node(pager, self.root, &root)?;
                pager.tracer().event(&Event::RootGrow {
                    root: self.root,
                    left,
                    right,
                });
            }
            Outcome::Underfull => {
                // A root may be underfull, but an internal root with a single
                // child is a wasted level: pull the child up into the root page.
                if let Node::Internal { keys, children } = read_node(pager, self.root)?
                    && keys.is_empty()
                {
                    let child = children[0];
                    let node = read_node(pager, child)?;
                    write_node(pager, self.root, &node)?;
                    pager.free(child)?;
                    pager.tracer().event(&Event::RootShrink {
                        root: self.root,
                        child,
                    });
                }
            }
        }
        Ok(())
    }
}

fn free_subtree(pager: &mut Pager, page: PageId) -> Result<()> {
    if let Node::Internal { children, .. } = read_node(pager, page)? {
        for child in children {
            free_subtree(pager, child)?;
        }
    }
    pager.free(page)
}

/// Which child of an internal node can hold `key`: the number of separator
/// keys at or below it. (Child `i` holds keys in `keys[i-1]..keys[i]`.)
fn child_index(keys: &[Vec<u8>], key: &[u8]) -> usize {
    keys.partition_point(|k| k.as_slice() <= key)
}

fn read_node(pager: &mut Pager, page: PageId) -> Result<Node> {
    pager.tracer().event(&Event::NodeVisit { page });
    let bytes = pager.read_page(page)?;
    Node::decode(&bytes).map_err(|err| match err {
        Error::Corrupt(msg) => Error::Corrupt(format!("page {page}: {msg}")),
        other => other,
    })
}

fn write_node(pager: &mut Pager, page: PageId, node: &Node) -> Result<()> {
    pager.write_page(page, &node.encode())
}
