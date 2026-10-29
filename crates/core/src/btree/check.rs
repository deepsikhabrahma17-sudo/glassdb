//! Looking at a whole tree: verify its invariants, and draw it as text.
//! Both read every page, so they are for tests, debugging and the tours.

use super::node::{MIN_FILL, Node};
use super::{BTree, read_node};
use crate::error::{Error, Result};
use crate::pager::{PageId, Pager};

/// Shape of a tree, as measured by `BTree::check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TreeStats {
    /// Levels, counting the root; a lone leaf is height 1.
    pub height: usize,
    pub internal_nodes: usize,
    pub leaves: usize,
    pub entries: usize,
}

impl BTree {
    /// Walk the whole tree and verify every invariant:
    /// - keys are sorted within each node, and every key lies between the
    ///   separators that led to its node;
    /// - all leaves are at the same depth;
    /// - no node except the root is underfull, and internal nodes have keys;
    /// - the leaf chain visits every leaf, left to right, and then ends.
    pub fn check(&self, pager: &mut Pager) -> Result<TreeStats> {
        let mut walk = Walk::default();
        walk.visit(pager, self.root, None, None, 1)?;

        let mut chain = Vec::new();
        let mut page = walk.leaves[0];
        while page != 0 && chain.len() <= walk.leaves.len() {
            chain.push(page);
            page = match read_node(pager, page)? {
                Node::Leaf { next, .. } => next,
                Node::Internal { .. } => 0,
            };
        }
        if chain != walk.leaves {
            return Err(Error::Corrupt(format!(
                "leaf chain {chain:?} does not match the leaves in key order {:?}",
                walk.leaves
            )));
        }
        Ok(walk.stats)
    }

    /// The tree as indented text, one node per line, for example:
    ///
    /// ```text
    /// page 1 (root) internal: k0002
    ///   page 3 leaf: k0000 k0001 → page 2
    ///   page 2 leaf: k0002 k0003 k0004 → end
    /// ```
    pub fn dump(&self, pager: &mut Pager) -> Result<String> {
        let mut out = String::new();
        dump_node(pager, self.root, 0, &mut out)?;
        Ok(out)
    }
}

#[derive(Default)]
struct Walk {
    stats: TreeStats,
    /// Depth of the first leaf found; every other leaf must match it.
    leaf_depth: Option<usize>,
    /// Leaf pages in key order, as found by the walk.
    leaves: Vec<PageId>,
}

impl Walk {
    /// Check the subtree at `page`, whose keys must lie in `low..high`.
    fn visit(
        &mut self,
        pager: &mut Pager,
        page: PageId,
        low: Option<&[u8]>,
        high: Option<&[u8]>,
        depth: usize,
    ) -> Result<()> {
        let fail = |msg: String| Err(Error::Corrupt(format!("page {page}: {msg}")));
        let node = read_node(pager, page)?;
        let is_root = depth == 1;
        if !is_root && node.size() < MIN_FILL {
            return fail(format!("underfull at {} bytes", node.size()));
        }
        let keys: Vec<&[u8]> = match &node {
            Node::Leaf { entries, .. } => entries.iter().map(|(k, _)| k.as_slice()).collect(),
            Node::Internal { keys, .. } => keys.iter().map(Vec::as_slice).collect(),
        };
        if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
            return fail("keys out of order".into());
        }
        if let (Some(low), Some(&first)) = (low, keys.first())
            && first < low
        {
            return fail(format!("key {first:?} is below its lower bound {low:?}"));
        }
        if let (Some(high), Some(&last)) = (high, keys.last())
            && last >= high
        {
            return fail(format!(
                "key {last:?} is not below its upper bound {high:?}"
            ));
        }

        match &node {
            Node::Leaf { entries, .. } => {
                match self.leaf_depth {
                    None => self.leaf_depth = Some(depth),
                    Some(d) if d != depth => {
                        return fail(format!("leaf at depth {depth}, other leaves at {d}"));
                    }
                    Some(_) => {}
                }
                self.stats.height = depth;
                self.stats.leaves += 1;
                self.stats.entries += entries.len();
                self.leaves.push(page);
            }
            Node::Internal { keys, children } => {
                if keys.is_empty() || children.len() != keys.len() + 1 {
                    return fail(format!(
                        "{} keys but {} children",
                        keys.len(),
                        children.len()
                    ));
                }
                self.stats.internal_nodes += 1;
                for (i, &child) in children.iter().enumerate() {
                    let child_low = if i == 0 {
                        low
                    } else {
                        Some(keys[i - 1].as_slice())
                    };
                    let child_high = keys.get(i).map(Vec::as_slice).or(high);
                    self.visit(pager, child, child_low, child_high, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

fn dump_node(pager: &mut Pager, page: PageId, depth: usize, out: &mut String) -> Result<()> {
    let indent = "  ".repeat(depth);
    let root = if depth == 0 { " (root)" } else { "" };
    match read_node(pager, page)? {
        Node::Internal { keys, children } => {
            let keys: Vec<String> = keys.iter().map(|k| show_key(k)).collect();
            out.push_str(&format!(
                "{indent}page {page}{root} internal: {}\n",
                keys.join(" | ")
            ));
            for child in children {
                dump_node(pager, child, depth + 1, out)?;
            }
        }
        Node::Leaf { entries, next } => {
            const SHOWN: usize = 8;
            let mut keys: Vec<String> = entries
                .iter()
                .take(SHOWN)
                .map(|(k, _)| show_key(k))
                .collect();
            if entries.len() > SHOWN {
                keys.push(format!("… (+{} more)", entries.len() - SHOWN));
            }
            if keys.is_empty() {
                keys.push("(empty)".into());
            }
            let next = if next == 0 {
                "end".to_string()
            } else {
                format!("page {next}")
            };
            out.push_str(&format!(
                "{indent}page {page}{root} leaf: {} → {next}\n",
                keys.join(" ")
            ));
        }
    }
    Ok(())
}

/// A key as readable text: printable ASCII as-is (shortened if long),
/// anything else as hex.
pub fn show_key(key: &[u8]) -> String {
    const MAX: usize = 16;
    if key.iter().all(|b| (0x20..0x7f).contains(b)) {
        let text = String::from_utf8_lossy(key);
        if key.len() <= MAX {
            format!("{text}")
        } else {
            format!("{}…", &text[..MAX - 3])
        }
    } else {
        let hex: String = key.iter().take(8).map(|b| format!("{b:02x}")).collect();
        let more = if key.len() > 8 { "…" } else { "" };
        format!("0x{hex}{more}")
    }
}
