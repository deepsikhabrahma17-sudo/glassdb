//! The one recursive walk behind both insert and delete.
//!
//! Each call changes its subtree, writes its own node back, and tells its
//! parent whether anything needs repairing: a split to absorb, or an
//! underfull child to merge or refill. Repairs can ripple up level by level.

use super::node::{MIN_FILL, Node};
use super::rebalance::{rebalance, split};
use super::{child_index, read_node, write_node};
use crate::error::Result;
use crate::pager::{PAGE_SIZE, PageId, Pager};
use crate::trace::Event;

pub(super) enum Change<'a> {
    Insert(&'a [u8]),
    Delete,
}

/// What a node reports to its parent after a change.
pub(super) enum Outcome {
    /// Nothing for the parent to do.
    Fine,
    /// The node overflowed and split. Its page kept the left half; the parent
    /// must add `separator` and the `right` page just after it.
    Split { separator: Vec<u8>, right: PageId },
    /// The node is below `MIN_FILL`; the parent should merge or refill it.
    Underfull,
}

/// Apply `change` to `key` in the subtree rooted at `page`.
/// Returns the key's old value (if any) and what the parent must repair.
pub(super) fn modify(
    pager: &mut Pager,
    page: PageId,
    key: &[u8],
    change: &Change,
) -> Result<(Option<Vec<u8>>, Outcome)> {
    let mut node = read_node(pager, page)?;
    let old = match &mut node {
        Node::Leaf { entries, .. } => {
            let found = entries.binary_search_by(|(k, _)| k.as_slice().cmp(key));
            match (change, found) {
                (Change::Insert(value), Ok(i)) => {
                    Some(std::mem::replace(&mut entries[i].1, value.to_vec()))
                }
                (Change::Insert(value), Err(i)) => {
                    entries.insert(i, (key.to_vec(), value.to_vec()));
                    None
                }
                (Change::Delete, Ok(i)) => Some(entries.remove(i).1),
                // Deleting a missing key changes nothing; don't even rewrite the page.
                (Change::Delete, Err(_)) => return Ok((None, Outcome::Fine)),
            }
        }
        Node::Internal { keys, children } => {
            let i = child_index(keys, key);
            let (old, outcome) = modify(pager, children[i], key, change)?;
            match outcome {
                // The child dealt with it; this node is unchanged.
                Outcome::Fine => return Ok((old, Outcome::Fine)),
                Outcome::Split { separator, right } => {
                    keys.insert(i, separator);
                    children.insert(i + 1, right);
                }
                Outcome::Underfull => rebalance(pager, keys, children, i)?,
            }
            old
        }
    };
    let outcome = store(pager, page, node)?;
    Ok((old, outcome))
}

/// Write `node` back to `page`, splitting it first if it no longer fits.
fn store(pager: &mut Pager, page: PageId, node: Node) -> Result<Outcome> {
    let size = node.size();
    if size <= PAGE_SIZE {
        write_node(pager, page, &node)?;
        return Ok(if size < MIN_FILL {
            Outcome::Underfull
        } else {
            Outcome::Fine
        });
    }
    // One extra entry or a longer separator can overflow a node by at most
    // one cell, so a single split always leaves two halves that fit.
    let right = pager.allocate()?;
    let (left_half, separator, right_half) = split(node, right);
    write_node(pager, page, &left_half)?;
    write_node(pager, right, &right_half)?;
    pager.tracer().event(&Event::NodeSplit {
        page,
        new_page: right,
        separator: separator.clone(),
    });
    Ok(Outcome::Split { separator, right })
}
