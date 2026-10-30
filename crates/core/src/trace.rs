//! The glass-box event stream.
//!
//! Every layer reports what it does as small `Event`s sent to a `Tracer`.
//! Normally the tracer is `NoopTracer` and events cost one empty call; the
//! visualizer (and `.trace on` in the CLI) plug in a tracer that records them.

use std::sync::Mutex;

use crate::pager::PageId;

/// One thing that happened inside the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A page was read from storage into the cache.
    PageRead { page: PageId },
    /// A page was written from the cache (or the header) to storage.
    PageWrite { page: PageId },
    /// A requested page was already in the cache.
    CacheHit { page: PageId },
    /// A requested page was not in the cache.
    CacheMiss { page: PageId },
    /// A page was dropped from the cache to make room; `dirty` means it had
    /// changes that must be written back.
    CacheEvict { page: PageId, dirty: bool },

    /// A B+ tree node was read on the way to a key.
    NodeVisit { page: PageId },
    /// A full node split: `page` kept the left half, `new_page` took the right,
    /// and `separator` was added to the parent.
    NodeSplit {
        page: PageId,
        new_page: PageId,
        separator: Vec<u8>,
    },
    /// An underfull node and its neighbour were merged into `left`; `right` was freed.
    NodeMerge { left: PageId, right: PageId },
    /// Two neighbours too full to merge shared their entries evenly instead.
    NodeBorrow { left: PageId, right: PageId },
    /// The root split: its two halves moved to `left` and `right`, and the
    /// root page became their parent. The tree grew one level.
    RootGrow {
        root: PageId,
        left: PageId,
        right: PageId,
    },
    /// The root had a single child, which was copied into the root page and
    /// freed. The tree shrank one level.
    RootShrink { root: PageId, child: PageId },

    /// A changed page was appended to the log.
    WalAppend { page: PageId },
    /// A page was read from the log, because the main file's copy is out of date.
    WalRead { page: PageId },
    /// A transaction's commit record reached disk: it now survives any crash.
    WalCommit { frames: u32 },
    /// The log's pages were copied into the main file and the log was emptied.
    Checkpoint { pages: u32 },
    /// Opening found a log left by a crash. `transactions` and `frames` were
    /// kept; `discarded` frames after the last commit record were thrown away.
    WalRecovered {
        transactions: usize,
        frames: usize,
        discarded: usize,
    },

    /// A SQL statement started running (shown as tidy, canonical SQL).
    Statement { sql: String },
    /// The statement finished: a summary such as "inserted 2 rows" or "error: ...".
    StatementDone { summary: String },
}

/// Receives engine events. The default method ignores them.
pub trait Tracer: Send + Sync {
    fn event(&self, _event: &Event) {}
}

/// A tracer that does nothing: the normal, zero-overhead setting.
pub struct NoopTracer;

impl Tracer for NoopTracer {}

/// A tracer that keeps every event in memory, for tests and the CLI.
#[derive(Default)]
pub struct RecordingTracer {
    events: Mutex<Vec<Event>>,
}

impl RecordingTracer {
    /// Return the events recorded so far and clear the list.
    pub fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl Tracer for RecordingTracer {
    fn event(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_tracer_keeps_events_in_order_and_take_clears() {
        let tracer = RecordingTracer::default();
        tracer.event(&Event::CacheMiss { page: 3 });
        tracer.event(&Event::PageRead { page: 3 });
        assert_eq!(
            tracer.take(),
            vec![Event::CacheMiss { page: 3 }, Event::PageRead { page: 3 }]
        );
        assert!(tracer.take().is_empty());
    }
}
