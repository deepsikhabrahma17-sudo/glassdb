//! B+ tree nodes: the in-memory `Node` and its on-page layout.
//!
//! Every node is one page, stored as a *slotted page*:
//!
//! | bytes      | field                                                    |
//! |------------|----------------------------------------------------------|
//! | 0          | kind: 1 = leaf, 2 = internal                             |
//! | 1..3       | cell count                                               |
//! | 3..5       | offset where the cell area starts                        |
//! | 5..9       | leaf: next leaf page (0 = none); internal: rightmost child |
//! | 9..12      | reserved                                                 |
//! | 12..       | slot array: one u16 cell offset per cell, in key order   |
//! | ...end     | cells, packed backwards from the end of the page         |
//!
//! Leaf cell: `key_len u16, value_len u16, key, value`.
//! Internal cell: `child u32, key_len u16, key`, where `child` holds keys below `key`.
//!
//! Nodes are decoded into a `Node`, changed as ordinary vectors, and encoded
//! back packed tight, so there is never fragmented free space to manage.

use crate::bytes::{get_u16, get_u32, put_u16, put_u32};
use crate::error::{Error, Result};
use crate::pager::{PAGE_SIZE, Page, PageId};

pub const KIND_LEAF: u8 = 1;
pub const KIND_INTERNAL: u8 = 2;
pub const HEADER_SIZE: usize = 12;
const SLOT_SIZE: usize = 2;

/// Longest key allowed, so internal nodes always hold several keys.
pub const MAX_KEY: usize = 512;
/// Longest key + value allowed, so every leaf holds at least 4 entries.
pub const MAX_ENTRY: usize = 1000;
/// A node other than the root with fewer bytes than this is underfull and
/// gets merged with or refilled from a neighbour.
pub const MIN_FILL: usize = PAGE_SIZE / 4;

/// One key/value pair in a leaf.
pub type Entry = (Vec<u8>, Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Entries sorted by key, and the next leaf to the right (0 = none).
    Leaf { entries: Vec<Entry>, next: PageId },
    /// `children[i]` holds keys below `keys[i]` (and at or above `keys[i - 1]`);
    /// the last child holds keys at or above the last key. There is always
    /// one more child than keys.
    Internal {
        keys: Vec<Vec<u8>>,
        children: Vec<PageId>,
    },
}

/// Bytes a leaf entry takes on the page, including its slot.
pub fn leaf_cell_size(key: &[u8], value: &[u8]) -> usize {
    SLOT_SIZE + 4 + key.len() + value.len()
}

/// Bytes an internal key (with its left child) takes on the page, including its slot.
pub fn internal_cell_size(key: &[u8]) -> usize {
    SLOT_SIZE + 6 + key.len()
}

impl Node {
    pub fn empty_leaf() -> Node {
        Node::Leaf {
            entries: Vec::new(),
            next: 0,
        }
    }

    /// Encoded size in bytes. More than `PAGE_SIZE` means the node must split.
    pub fn size(&self) -> usize {
        let cells: usize = match self {
            Node::Leaf { entries, .. } => entries.iter().map(|(k, v)| leaf_cell_size(k, v)).sum(),
            Node::Internal { keys, .. } => keys.iter().map(|k| internal_cell_size(k)).sum(),
        };
        HEADER_SIZE + cells
    }

    /// Lay the node out as a page. Panics if it doesn't fit.
    pub fn encode(&self) -> Box<Page> {
        assert!(
            self.size() <= PAGE_SIZE,
            "node of {} bytes does not fit in a page",
            self.size()
        );
        let mut page = Box::new([0u8; PAGE_SIZE]);
        // Cells are packed backwards from the end; `end` is where the last one starts.
        let mut end = PAGE_SIZE;
        let count = match self {
            Node::Leaf { entries, next } => {
                page[0] = KIND_LEAF;
                put_u32(&mut page[..], 5, *next);
                for (i, (key, value)) in entries.iter().enumerate() {
                    end -= 4 + key.len() + value.len();
                    put_u16(&mut page[..], end, key.len() as u16);
                    put_u16(&mut page[..], end + 2, value.len() as u16);
                    let key_end = end + 4 + key.len();
                    page[end + 4..key_end].copy_from_slice(key);
                    page[key_end..key_end + value.len()].copy_from_slice(value);
                    put_u16(&mut page[..], HEADER_SIZE + i * SLOT_SIZE, end as u16);
                }
                entries.len()
            }
            Node::Internal { keys, children } => {
                page[0] = KIND_INTERNAL;
                // The last child has no key of its own, so it lives in the header.
                put_u32(
                    &mut page[..],
                    5,
                    *children.last().expect("internal node has a child"),
                );
                for (i, (key, &child)) in keys.iter().zip(children).enumerate() {
                    end -= 6 + key.len();
                    put_u32(&mut page[..], end, child);
                    put_u16(&mut page[..], end + 4, key.len() as u16);
                    page[end + 6..end + 6 + key.len()].copy_from_slice(key);
                    put_u16(&mut page[..], HEADER_SIZE + i * SLOT_SIZE, end as u16);
                }
                keys.len()
            }
        };
        put_u16(&mut page[..], 1, count as u16);
        put_u16(&mut page[..], 3, end as u16);
        page
    }

    /// Read a node back from a page, checking every offset and length so a
    /// damaged page gives `Corrupt` instead of a panic.
    pub fn decode(page: &Page) -> Result<Node> {
        let count = get_u16(&page[..], 1) as usize;
        let link = get_u32(&page[..], 5);
        let slots_end = HEADER_SIZE + count * SLOT_SIZE;
        if slots_end > PAGE_SIZE {
            return Err(corrupt(format!("{count} cells cannot fit in a page")));
        }
        // Where cell `i` starts; it must lie between the slot array and the page end.
        let cell_at = |i: usize| -> Result<usize> {
            let at = get_u16(&page[..], HEADER_SIZE + i * SLOT_SIZE) as usize;
            if at < slots_end || at >= PAGE_SIZE {
                return Err(corrupt(format!("cell {i} has bad offset {at}")));
            }
            Ok(at)
        };
        match page[0] {
            KIND_LEAF => {
                let mut entries = Vec::with_capacity(count);
                for i in 0..count {
                    let at = cell_at(i)?;
                    let lens = bytes(page, at, 4)?;
                    let key_len = get_u16(lens, 0) as usize;
                    let value_len = get_u16(lens, 2) as usize;
                    let key = bytes(page, at + 4, key_len)?.to_vec();
                    let value = bytes(page, at + 4 + key_len, value_len)?.to_vec();
                    entries.push((key, value));
                }
                Ok(Node::Leaf {
                    entries,
                    next: link,
                })
            }
            KIND_INTERNAL => {
                let mut keys = Vec::with_capacity(count);
                let mut children = Vec::with_capacity(count + 1);
                for i in 0..count {
                    let at = cell_at(i)?;
                    let head = bytes(page, at, 6)?;
                    children.push(get_u32(head, 0));
                    let key_len = get_u16(head, 4) as usize;
                    keys.push(bytes(page, at + 6, key_len)?.to_vec());
                }
                children.push(link);
                Ok(Node::Internal { keys, children })
            }
            kind => Err(corrupt(format!("unknown node kind {kind}"))),
        }
    }
}

/// `len` bytes of `page` from `start`, or `Corrupt` if that runs off the end.
fn bytes(page: &Page, start: usize, len: usize) -> Result<&[u8]> {
    page.get(start..start + len)
        .ok_or_else(|| corrupt(format!("cell at {start} runs past the page end")))
}

fn corrupt(msg: String) -> Error {
    Error::Corrupt(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(entries: &[(&str, &str)], next: PageId) -> Node {
        Node::Leaf {
            entries: entries
                .iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
            next,
        }
    }

    fn internal(keys: &[&str], children: &[PageId]) -> Node {
        Node::Internal {
            keys: keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
            children: children.to_vec(),
        }
    }

    #[test]
    fn round_trips() {
        for node in [
            Node::empty_leaf(),
            leaf(&[("a", "1"), ("bb", ""), ("ccc", "three")], 9),
            internal(&["m"], &[3, 4]),
            internal(&["b", "k", "x"], &[5, 6, 7, 8]),
        ] {
            assert_eq!(Node::decode(&node.encode()).unwrap(), node);
        }
    }

    #[test]
    fn leaf_layout_matches_design_doc() {
        let page = leaf(&[("ab", "xyz")], 7).encode();
        // cell = key_len(2) + value_len(2) + "ab" + "xyz" = 9 bytes, at the very end
        let cell = PAGE_SIZE - 9;
        assert_eq!(page[0], KIND_LEAF);
        assert_eq!(page[1..3], 1u16.to_le_bytes()); // one cell
        assert_eq!(page[3..5], (cell as u16).to_le_bytes()); // cell area start
        assert_eq!(page[5..9], 7u32.to_le_bytes()); // next leaf
        assert_eq!(page[12..14], (cell as u16).to_le_bytes()); // slot 0
        assert_eq!(page[cell..cell + 2], 2u16.to_le_bytes());
        assert_eq!(page[cell + 2..cell + 4], 3u16.to_le_bytes());
        assert_eq!(&page[cell + 4..], b"abxyz");
    }

    #[test]
    fn internal_layout_matches_design_doc() {
        let page = internal(&["m"], &[3, 4]).encode();
        // cell = child(4) + key_len(2) + "m" = 7 bytes
        let cell = PAGE_SIZE - 7;
        assert_eq!(page[0], KIND_INTERNAL);
        assert_eq!(page[1..3], 1u16.to_le_bytes());
        assert_eq!(page[5..9], 4u32.to_le_bytes()); // rightmost child
        assert_eq!(page[12..14], (cell as u16).to_le_bytes());
        assert_eq!(page[cell..cell + 4], 3u32.to_le_bytes()); // left child of "m"
        assert_eq!(page[cell + 4..cell + 6], 1u16.to_le_bytes());
        assert_eq!(&page[cell + 6..], b"m");
    }

    #[test]
    fn size_matches_the_encoded_bytes() {
        for node in [
            Node::empty_leaf(),
            leaf(&[("a", "1"), ("bb", "22")], 0),
            internal(&["b", "k"], &[5, 6, 7]),
        ] {
            let page = node.encode();
            let count = get_u16(&page[..], 1) as usize;
            let cell_start = get_u16(&page[..], 3) as usize;
            let used = HEADER_SIZE + count * SLOT_SIZE + (PAGE_SIZE - cell_start);
            assert_eq!(node.size(), used, "{node:?}");
        }
        assert_eq!(Node::empty_leaf().size(), HEADER_SIZE);
    }

    #[test]
    fn rejects_unknown_kind_and_bad_offsets() {
        let mut page = leaf(&[("a", "1")], 0).encode();
        page[0] = 9;
        assert!(matches!(Node::decode(&page), Err(Error::Corrupt(_))));

        let mut page = leaf(&[("a", "1")], 0).encode();
        page[12..14].copy_from_slice(&5u16.to_le_bytes()); // slot points into the header
        assert!(matches!(Node::decode(&page), Err(Error::Corrupt(_))));

        let mut page = leaf(&[("a", "1")], 0).encode();
        page[1..3].copy_from_slice(&3000u16.to_le_bytes()); // slot array past the page
        assert!(matches!(Node::decode(&page), Err(Error::Corrupt(_))));

        let mut page = leaf(&[("a", "1")], 0).encode();
        let cell = PAGE_SIZE - 6;
        page[cell..cell + 2].copy_from_slice(&900u16.to_le_bytes()); // key runs off the end
        assert!(matches!(Node::decode(&page), Err(Error::Corrupt(_))));
    }
}
