//! Page 0: the file header.
//!
//! Layout (all integers little-endian):
//!
//! | bytes  | field                                       |
//! |--------|---------------------------------------------|
//! | 0..8   | magic `GLASSDB\0`                           |
//! | 8..10  | format version                              |
//! | 10..12 | page size                                   |
//! | 12..16 | page count, including this header page      |
//! | 16..20 | first free page (0 = free list is empty)    |
//! | 20..24 | number of free pages                        |
//! | 24..32 | history floor: oldest txn `AS OF` can reach |
//! | 32..   | zero                                        |

use super::{PAGE_SIZE, Page, PageId};
use crate::bytes::{get_u32, put_u32};
use crate::error::{Error, Result};

pub const MAGIC: [u8; 8] = *b"GLASSDB\0";
pub const FORMAT_VERSION: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub page_count: u32,
    pub free_head: PageId,
    pub free_count: u32,
    /// `VACUUM HISTORY` removed what any older transaction would need.
    pub history_floor: u64,
}

impl Header {
    /// The header of an empty database: just itself, no free pages.
    pub fn new_database() -> Self {
        Header {
            page_count: 1,
            free_head: 0,
            free_count: 0,
            history_floor: 0,
        }
    }

    pub fn encode(&self) -> Box<Page> {
        let mut page = Box::new([0u8; PAGE_SIZE]);
        page[0..8].copy_from_slice(&MAGIC);
        page[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        page[10..12].copy_from_slice(&(PAGE_SIZE as u16).to_le_bytes());
        put_u32(&mut page[..], 12, self.page_count);
        put_u32(&mut page[..], 16, self.free_head);
        put_u32(&mut page[..], 20, self.free_count);
        page[24..32].copy_from_slice(&self.history_floor.to_le_bytes());
        page
    }

    /// Parse and sanity-check a header page.
    pub fn decode(page: &Page) -> Result<Header> {
        if page[0..8] != MAGIC {
            return Err(Error::BadMagic);
        }
        let version = u16::from_le_bytes([page[8], page[9]]);
        if version != FORMAT_VERSION {
            return Err(Error::UnsupportedVersion(version));
        }
        let page_size = u16::from_le_bytes([page[10], page[11]]);
        if page_size as usize != PAGE_SIZE {
            return Err(Error::BadPageSize(page_size));
        }
        let header = Header {
            page_count: get_u32(&page[..], 12),
            free_head: get_u32(&page[..], 16),
            free_count: get_u32(&page[..], 20),
            history_floor: u64::from_le_bytes(page[24..32].try_into().unwrap()),
        };
        header.check()?;
        Ok(header)
    }

    /// Reject counts that cannot describe a real file.
    fn check(&self) -> Result<()> {
        let corrupt = |msg: String| Err(Error::Corrupt(msg));
        if self.page_count == 0 {
            return corrupt("page count is 0 (the header page itself is missing)".into());
        }
        if self.free_head >= self.page_count {
            return corrupt(format!(
                "free list starts at page {} but there are only {} pages",
                self.free_head, self.page_count
            ));
        }
        // An empty list (head 0) must have count 0, and a non-empty list a count of at least 1.
        if (self.free_head == 0) != (self.free_count == 0) {
            return corrupt(format!(
                "free list head {} does not match free count {}",
                self.free_head, self.free_count
            ));
        }
        // Page 0 can never be free, so at most page_count - 1 pages are.
        if self.free_count >= self.page_count {
            return corrupt(format!(
                "{} free pages but only {} pages",
                self.free_count, self.page_count
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            page_count: 42,
            free_head: 7,
            free_count: 3,
            history_floor: 9,
        }
    }

    #[test]
    fn round_trip() {
        assert_eq!(Header::decode(&sample().encode()).unwrap(), sample());
        let fresh = Header::new_database();
        assert_eq!(fresh.page_count, 1);
        assert_eq!(Header::decode(&fresh.encode()).unwrap(), fresh);
    }

    #[test]
    fn byte_layout_matches_design_doc() {
        let page = Header {
            page_count: 0x0102_0304,
            free_head: 5,
            free_count: 1,
            history_floor: 0x0A0B,
        }
        .encode();
        assert_eq!(&page[0..8], b"GLASSDB\0");
        assert_eq!(page[8..10], [2, 0]);
        assert_eq!(page[10..12], 4096u16.to_le_bytes());
        assert_eq!(page[12..16], [4, 3, 2, 1]);
        assert_eq!(page[16..20], [5, 0, 0, 0]);
        assert_eq!(page[20..24], [1, 0, 0, 0]);
        assert_eq!(page[24..32], [0x0B, 0x0A, 0, 0, 0, 0, 0, 0]);
        assert!(page[32..].iter().all(|&b| b == 0));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut page = sample().encode();
        page[0] = b'X';
        assert!(matches!(Header::decode(&page), Err(Error::BadMagic)));
    }

    #[test]
    fn rejects_other_version_and_page_size() {
        let mut page = sample().encode();
        page[8..10].copy_from_slice(&1u16.to_le_bytes());
        assert!(matches!(
            Header::decode(&page),
            Err(Error::UnsupportedVersion(1))
        ));

        let mut page = sample().encode();
        page[10..12].copy_from_slice(&8192u16.wrapping_sub(1).to_le_bytes());
        assert!(matches!(Header::decode(&page), Err(Error::BadPageSize(_))));
    }

    #[test]
    fn rejects_inconsistent_counts() {
        let bad = [
            // no pages at all, not even the header
            Header {
                page_count: 0,
                free_head: 0,
                free_count: 0,
                history_floor: 0,
            },
            // free list points past the end
            Header {
                page_count: 5,
                free_head: 5,
                free_count: 1,
                history_floor: 0,
            },
            // non-empty list with a zero count
            Header {
                page_count: 5,
                free_head: 2,
                free_count: 0,
                history_floor: 0,
            },
            // empty list with a non-zero count
            Header {
                page_count: 5,
                free_head: 0,
                free_count: 2,
                history_floor: 0,
            },
            // more free pages than pages
            Header {
                page_count: 5,
                free_head: 2,
                free_count: 5,
                history_floor: 0,
            },
        ];
        for header in bad {
            assert!(
                matches!(Header::decode(&header.encode()), Err(Error::Corrupt(_))),
                "accepted {header:?}"
            );
        }
    }
}
