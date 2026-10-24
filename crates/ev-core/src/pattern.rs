//! Name / path matching primitives: substring, prefix, suffix, exact, glob and
//! regular expressions, all with an ASCII case-insensitive fast path.

use std::sync::Arc;

use crate::index::{Id, Index};
use crate::regex::{self, Regex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Name,
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Contains,
    Prefix,
    Suffix,
    Exact,
    Glob,
    Regex,
}

#[derive(Debug, Clone)]
pub struct NameMatcher {
    pub text: String,
    /// Lowercased needle bytes (or the raw bytes when `case` is set).
    needle: Vec<u8>,
    /// Lowercased pattern characters, used by the glob engine.
    pattern: Vec<char>,
    pub kind: Kind,
    pub target: Target,
    pub case: bool,
    pub whole_word: bool,
    regex: Option<Arc<Regex>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MatchOptions {
    pub case: bool,
    pub whole_word: bool,
    pub wildcards: bool,
    pub regex: bool,
    pub whole_filename: bool,
    pub start_with: bool,
    pub end_with: bool,
}

#[inline]
pub fn lower_byte(b: u8) -> u8 {
    b.to_ascii_lowercase()
}

#[inline]
pub fn lower_char(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

pub fn ascii_lower(s: &str) -> String {
    s.chars().map(lower_char).collect()
}

impl NameMatcher {
    pub fn new(text: &str, target: Target, opts: MatchOptions) -> Result<Self, String> {
        let mut kind = if opts.regex {
            Kind::Regex
        } else if opts.start_with {
            Kind::Prefix
        } else if opts.end_with {
            Kind::Suffix
        } else if opts.whole_filename {
            if opts.wildcards && text.contains(['*', '?']) {
                Kind::Glob
            } else {
                Kind::Exact
            }
        } else if opts.wildcards && text.contains(['*', '?']) {
            Kind::Glob
        } else {
            Kind::Contains
        };

        let mut regex = None;
        let needle_src = if opts.case {
            text.to_string()
        } else {
            ascii_lower(text)
        };
        if kind == Kind::Regex {
            match regex::compile(&needle_src) {
                Ok(r) => regex = Some(r),
                Err(e) => return Err(format!("invalid regex: {e}")),
            }
        }
        // A pattern that is only wildcards is handled as "match anything".
        if kind == Kind::Glob && needle_src.chars().all(|c| c == '*' || c == '?') {
            kind = Kind::Contains;
        }

        let mut m = Self {
            text: text.to_string(),
            needle: needle_src.as_bytes().to_vec(),
            pattern: needle_src.chars().collect(),
            kind,
            target,
            case: opts.case,
            whole_word: opts.whole_word,
            regex,
        };
        if m.kind == Kind::Contains && m.needle.is_empty() {
            m.kind = Kind::Exact;
        }
        Ok(m)
    }

    /// A cheap upper bound on how selective this term is (larger = more
    /// selective). Used to order AND terms so the expensive ones run rarely.
    pub fn selectivity(&self) -> usize {
        match self.kind {
            Kind::Contains | Kind::Glob | Kind::Regex => self.needle.len().max(1),
            Kind::Prefix | Kind::Suffix | Kind::Exact => self.needle.len().max(1) + 4,
        }
    }

    /// True when this term can use the dedicated scan loop (plain,
    /// case-insensitive substring on the file name).
    #[inline]
    pub fn is_fast(&self) -> bool {
        self.kind == Kind::Contains
            && self.target == Target::Name
            && !self.whole_word
            && !self.case
    }

    /// The lowercased needle for the specialised scan loop.
    #[inline]
    pub fn plain_needle(&self) -> Option<&[u8]> {
        if self.is_fast() && !self.needle.is_empty() {
            Some(&self.needle)
        } else {
            None
        }
    }

    #[inline]
    pub fn matches(&self, index: &Index, id: Id) -> bool {
        match self.target {
            Target::Name => {
                let hay = if self.case {
                    index.name(id)
                } else {
                    index.lower_name(id)
                };
                self.matches_str(hay)
            }
            Target::Path => {
                let hay = index.path(id);
                if self.case {
                    self.matches_str(hay)
                } else {
                    self.matches_str_ci(hay)
                }
            }
        }
    }

    #[inline]
    fn matches_str(&self, hay: &str) -> bool {
        match self.kind {
            Kind::Contains => {
                if self.whole_word {
                    find_whole_word(hay.as_bytes(), &self.needle)
                } else {
                    memchr::memmem::find(hay.as_bytes(), &self.needle).is_some()
                }
            }
            Kind::Prefix => hay.as_bytes().starts_with(&self.needle),
            Kind::Suffix => hay.as_bytes().ends_with(&self.needle),
            Kind::Exact => hay.as_bytes() == self.needle.as_slice(),
            Kind::Glob => {
                let chars: Vec<char> = hay.chars().collect();
                glob_match(&chars, &self.pattern)
            }
            Kind::Regex => self
                .regex
                .as_ref()
                .map(|r| r.is_match(hay))
                .unwrap_or(false),
        }
    }

    /// Case-insensitive matching used for paths, where we do not keep a
    /// lowercased copy of every string to save memory.
    #[inline]
    fn matches_str_ci(&self, hay: &str) -> bool {
        match self.kind {
            Kind::Contains => {
                if self.whole_word {
                    find_whole_word_ci(hay.as_bytes(), &self.needle)
                } else {
                    contains_ci(hay.as_bytes(), &self.needle)
                }
            }
            Kind::Prefix => starts_with_ci(hay.as_bytes(), &self.needle),
            Kind::Suffix => ends_with_ci(hay.as_bytes(), &self.needle),
            Kind::Exact => eq_ci(hay.as_bytes(), &self.needle),
            Kind::Glob => {
                let lowered = ascii_lower(hay);
                let chars: Vec<char> = lowered.chars().collect();
                glob_match(&chars, &self.pattern)
            }
            Kind::Regex => self
                .regex
                .as_ref()
                .map(|r| r.is_match(hay))
                .unwrap_or(false),
        }
    }
}

/// Case-insensitive substring search on raw bytes. `needle` must already be
/// lowercase. Uses `memchr` for the first byte in both cases so the scan stays
/// SIMD-accelerated even when the case is unknown.
pub fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    let first = needle[0];
    let upper = first.to_ascii_uppercase();
    let mut from = 0usize;
    while from + needle.len() <= hay.len() {
        let window = &hay[from..];
        let found = if first == upper {
            memchr::memchr(first, window)
        } else {
            memchr::memchr2(first, upper, window)
        };
        let Some(off) = found else { return false };
        let pos = from + off;
        if pos + needle.len() > hay.len() {
            return false;
        }
        if hay[pos..pos + needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return true;
        }
        from = pos + 1;
    }
    false
}

pub fn starts_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len()
        && hay[..needle.len()]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

pub fn ends_with_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len()
        && hay[hay.len() - needle.len()..]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

pub fn eq_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() == needle.len()
        && hay
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

pub fn find_whole_word(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(off) = memchr::memmem::find(&hay[from..], needle) {
        let start = from + off;
        let end = start + needle.len();
        let left_ok = start == 0 || !is_word_byte(hay[start - 1]);
        let right_ok = end == hay.len() || !is_word_byte(hay[end]);
        if left_ok && right_ok {
            return true;
        }
        from = start + 1;
        if from + needle.len() > hay.len() {
            break;
        }
    }
    false
}

fn find_whole_word_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let first = needle[0];
    let upper = first.to_ascii_uppercase();
    let mut from = 0usize;
    while from + needle.len() <= hay.len() {
        let window = &hay[from..];
        let found = if first == upper {
            memchr::memchr(first, window)
        } else {
            memchr::memchr2(first, upper, window)
        };
        let Some(off) = found else { return false };
        let start = from + off;
        let end = start + needle.len();
        if end > hay.len() {
            return false;
        }
        let ok = hay[start..end]
            .iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b);
        if ok {
            let left_ok = start == 0 || !is_word_byte(hay[start - 1]);
            let right_ok = end == hay.len() || !is_word_byte(hay[end]);
            if left_ok && right_ok {
                return true;
            }
        }
        from = start + 1;
    }
    false
}

/// Iterative glob matcher supporting `*` (any run) and `?` (one character).
pub fn glob_match(s: &[char], p: &[char]) -> bool {
    let (mut si, mut pi) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_si = 0usize;
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            si += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_si = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ci_contains() {
        assert!(contains_ci(b"Hello World", b"world"));
        assert!(contains_ci(b"ABC", b"abc"));
        assert!(!contains_ci(b"ABC", b"abd"));
        assert!(starts_with_ci(b"Hello", b"he"));
        assert!(ends_with_ci(b"Hello", b"lo"));
        assert!(eq_ci(b"Hello", b"hello"));
    }

    #[test]
    fn globs() {
        let p: Vec<char> = "*.rs".chars().collect();
        assert!(glob_match(&"main.rs".chars().collect::<Vec<_>>(), &p));
        assert!(!glob_match(&"main.rss".chars().collect::<Vec<_>>(), &p));
        let p: Vec<char> = "a?c".chars().collect();
        assert!(glob_match(&"abc".chars().collect::<Vec<_>>(), &p));
        assert!(!glob_match(&"abbc".chars().collect::<Vec<_>>(), &p));
    }

    #[test]
    fn whole_words() {
        assert!(find_whole_word(b"a foo b", b"foo"));
        assert!(!find_whole_word(b"afoob", b"foo"));
        assert!(find_whole_word_ci(b"a FOO b", b"foo"));
    }
}
