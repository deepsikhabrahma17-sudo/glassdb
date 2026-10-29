//! Reshaping nodes: splitting one that is too full, and merging or
//! refilling one that is too empty.
//!
//! Entries have different sizes, so every split point is chosen by bytes,
//! not by count: the two halves end up as close in size as possible.

use std::ops::RangeInclusive;

use super::node::{Entry, Node, internal_cell_size, leaf_cell_size};
use super::{read_node, write_node};
use crate::error::{Error, Result};
use crate::pager::{PAGE_SIZE, PageId, Pager};
use crate::trace::Event;

/// Split a node into (left half, separator, right half). The right half will
/// live at `right_page`, which the left half's leaf link must point to.
///
/// - Leaf: the separator is a *copy* of the right half's first key, because
///   every key must stay in a leaf.
/// - Internal: the middle key *moves up* to the parent and leaves this level.
pub(super) fn split(node: Node, right_page: PageId) -> (Node, Vec<u8>, Node) {
    match node {
        Node::Leaf { mut entries, next } => {
            let k = leaf_split_at(&entries);
            let right = entries.split_off(k);
            let separator = right[0].0.clone();
            let left = Node::Leaf {
                entries,
                next: right_page,
            };
            (
                left,
                separator,
                Node::Leaf {
                    entries: right,
                    next,
                },
            )
        }
        Node::Internal {
            mut keys,
            mut children,
        } => {
            let m = internal_split_at(&keys);
            let right_keys = keys.split_off(m + 1);
            let separator = keys.pop().expect("split point leaves a middle key");
            let right_children = children.split_off(m + 1);
            let left = Node::Internal { keys, children };
            let right = Node::Internal {
                keys: right_keys,
                children: right_children,
            };
            (left, separator, right)
        }
    }
}

/// `children[i]` of a parent is underfull. Pair it with a neighbour (the left
/// one if it has one) and either merge the two, if they fit in one page, or
/// share their entries evenly. Updates the parent's `keys` and `children`.
pub(super) fn rebalance(
    pager: &mut Pager,
    keys: &mut Vec<Vec<u8>>,
    children: &mut Vec<PageId>,
    i: usize,
) -> Result<()> {
    let li = if i > 0 { i - 1 } else { i };
    let (left_page, right_page) = (children[li], children[li + 1]);
    let left = read_node(pager, left_page)?;
    let right = read_node(pager, right_page)?;
    let merged = merge(left, &keys[li], right).ok_or_else(|| {
        Error::Corrupt(format!(
            "neighbours {left_page} and {right_page} are different node kinds"
        ))
    })?;

    if merged.size() <= PAGE_SIZE {
        // Merge: everything moves into the left page and the right page is freed.
        write_node(pager, left_page, &merged)?;
        keys.remove(li);
        children.remove(li + 1);
        pager.free(right_page)?;
        pager.tracer().event(&Event::NodeMerge {
            left: left_page,
            right: right_page,
        });
    } else {
        // Borrow: too much for one page, so split the combined entries evenly
        // back into the same two pages, with a new separator between them.
        let (left_half, separator, right_half) = split(merged, right_page);
        write_node(pager, left_page, &left_half)?;
        write_node(pager, right_page, &right_half)?;
        keys[li] = separator;
        pager.tracer().event(&Event::NodeBorrow {
            left: left_page,
            right: right_page,
        });
    }
    Ok(())
}

/// Join two neighbouring nodes into one (which may be too big for a page).
/// For internal nodes the parent's `separator` comes down between them.
/// Returns `None` if the two are not the same kind.
fn merge(left: Node, separator: &[u8], right: Node) -> Option<Node> {
    match (left, right) {
        (
            Node::Leaf { mut entries, .. },
            Node::Leaf {
                entries: rest,
                next,
            },
        ) => {
            entries.extend(rest);
            Some(Node::Leaf { entries, next })
        }
        (
            Node::Internal {
                mut keys,
                mut children,
            },
            Node::Internal {
                keys: right_keys,
                children: right_children,
            },
        ) => {
            keys.push(separator.to_vec());
            keys.extend(right_keys);
            children.extend(right_children);
            Some(Node::Internal { keys, children })
        }
        _ => None,
    }
}

/// Where to split leaf entries: the left half gets `entries[..k]`.
fn leaf_split_at(entries: &[Entry]) -> usize {
    let sums = prefix_sums(entries.iter().map(|(k, v)| leaf_cell_size(k, v)));
    let total = sums[entries.len()];
    closest_to_even(1..=entries.len() - 1, |k| (sums[k], total - sums[k]))
}

/// Which internal key moves up: left keeps `keys[..m]`, right gets `keys[m + 1..]`.
/// Both sides keep at least one key.
fn internal_split_at(keys: &[Vec<u8>]) -> usize {
    let sums = prefix_sums(keys.iter().map(|k| internal_cell_size(k)));
    let total = sums[keys.len()];
    closest_to_even(1..=keys.len() - 2, |m| (sums[m], total - sums[m + 1]))
}

/// The candidate whose (left bytes, right bytes) are closest to equal.
fn closest_to_even(
    candidates: RangeInclusive<usize>,
    halves: impl Fn(usize) -> (usize, usize),
) -> usize {
    candidates
        .min_by_key(|&k| {
            let (left, right) = halves(k);
            left.abs_diff(right)
        })
        .expect("a node being split has at least one split point")
}

/// `sums[i]` = total of the first `i` sizes.
fn prefix_sums(sizes: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut sums = vec![0];
    for size in sizes {
        sums.push(sums.last().unwrap() + size);
    }
    sums
}
