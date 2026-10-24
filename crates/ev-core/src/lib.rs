//! `ev-core` - the index, query language and search engine behind
//! everything-rs.
//!
//! The design goal is Everything's defining property: the whole file system
//! stays resident in a compact, cache-friendly structure so a query returns in
//! single-digit milliseconds on a million files.
//!
//! ```text
//!   ev-ntfs  -->  raw records  -->  Index  -->  Query  -->  search()
//!  (MFT/USN)                     (arenas)    (parser)     (scan+sort)
//! ```

pub mod date;
pub mod index;
pub mod pattern;
pub mod persist;
pub mod query;
pub mod regex;
pub mod search;

pub use date::{LocalClock, format_filetime, local_clock};
pub use index::{Entry, Id, Index, IndexBuilder, RawEntry, Stat, VolumeData, NONE};
pub use pattern::{Kind, MatchOptions, NameMatcher, Target};
pub use query::{DateField, DupeKind, Leaf, Node, Query, TypeClass};
pub use search::{Outcome, SortKey, SortSpec};

/// Human readable size, Everything-style (`1.5 MB`).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// `1234567` -> `1,234,567`
pub fn thousands(n: usize) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1536), "1.50 KB");
        assert_eq!(format_size(10 * 1024 * 1024), "10.0 MB");
        assert_eq!(format_size(200 * 1024 * 1024), "200 MB");
    }

    #[test]
    fn counts() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1000), "1,000");
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
