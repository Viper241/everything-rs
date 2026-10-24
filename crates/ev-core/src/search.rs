//! The search engine.
//!
//! Candidate collection is a single linear pass over the compact entry array.
//! The common case (one plain substring term, which is what happens on every
//! keystroke) takes a dedicated branch so it compiles down to a tight loop over
//! the lowercased name arena.

use std::cmp::Ordering;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use crate::index::{FLAG_DELETED, FLAG_ROOT, Id, Index};
use crate::pattern::NameMatcher;
use crate::query::{DateField, DateRange, DupeKind, Leaf, Node, Query, TypeClass, type_ext_list};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Path,
    Size,
    DateModified,
    DateCreated,
    DateAccessed,
    Ext,
    Type,
}

#[derive(Debug, Clone, Copy)]
pub struct SortSpec {
    pub key: SortKey,
    pub desc: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            desc: false,
        }
    }
}

impl SortSpec {
    pub fn parse(name: &str, desc: bool) -> Option<Self> {
        let key = match name.trim().to_ascii_lowercase().as_str() {
            "name" | "filename" => SortKey::Name,
            "path" => SortKey::Path,
            "size" => SortKey::Size,
            "datemodified" | "dm" | "date-modified" => SortKey::DateModified,
            "datecreated" | "dc" | "date-created" => SortKey::DateCreated,
            "dateaccessed" | "da" | "date-accessed" => SortKey::DateAccessed,
            "extension" | "ext" => SortKey::Ext,
            "type" => SortKey::Type,
            _ => return None,
        };
        Some(Self { key, desc })
    }

    fn needs_stats(&self) -> bool {
        matches!(
            self.key,
            SortKey::Size | SortKey::DateModified | SortKey::DateCreated | SortKey::DateAccessed
        )
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub ids: Vec<Id>,
    /// Number of matches before `count:`/display truncation.
    pub total: usize,
    pub elapsed: Duration,
    pub cancelled: bool,
    /// True when some rows had no metadata available yet.
    pub stats_incomplete: bool,
}

impl Index {
    pub fn search(
        &self,
        query: &Query,
        sort: SortSpec,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Outcome {
        let started = Instant::now();
        let mut ids: Vec<Id> = Vec::new();
        let mut cancelled = false;

        if query.is_match_all() {
            for id in 0..self.len() as Id {
                let e = self.entry(id);
                if e.is_deleted() || e.is_root() {
                    continue;
                }
                if let Some(dir_only) = query.only {
                    if self.is_dir(id) != dir_only {
                        continue;
                    }
                }
                ids.push(id);
            }
        } else if let Some(matcher) = plain_name_term(&query.root) {
            self.scan_plain(query, matcher, &mut ids, cancel);
        } else {
            for id in 0..self.len() as Id {
                if id & 0x3FFF == 0 && cancel.load(AtomicOrdering::Relaxed) {
                    cancelled = true;
                    break;
                }
                let e = self.entry(id);
                if e.is_deleted() || e.is_root() {
                    continue;
                }
                if let Some(dir_only) = query.only {
                    if self.is_dir(id) != dir_only {
                        continue;
                    }
                }
                if self.eval(&query.root, id) {
                    ids.push(id);
                }
            }
        }

        if !cancelled {
            if let Some(kind) = query.dupe {
                self.apply_dupe(&mut ids, kind);
            }
            let total = ids.len();
            if let Some(n) = query.limit {
                if ids.len() > n {
                    ids.truncate(n.max(1));
                }
            }
            let stats_incomplete = self.finish(&mut ids, sort, limit);
            return Outcome {
                ids,
                total,
                elapsed: started.elapsed(),
                cancelled: false,
                stats_incomplete,
            };
        }

        Outcome {
            ids,
            total: 0,
            elapsed: started.elapsed(),
            cancelled: true,
            stats_incomplete: false,
        }
    }

    fn scan_plain(&self, query: &Query, matcher: &NameMatcher, out: &mut Vec<Id>, cancel: &AtomicBool) {
        let Some(needle) = matcher.plain_needle() else {
            for id in 0..self.len() as u32 {
                if id & 0x3FFF == 0 && cancel.load(AtomicOrdering::Relaxed) {
                    return;
                }
                let e = self.entry(id);
                if e.is_deleted() || e.is_root() {
                    continue;
                }
                if let Some(dir_only) = query.only {
                    if self.is_dir(id) != dir_only {
                        continue;
                    }
                }
                if matcher.matches(self, id) {
                    out.push(id);
                }
            }
            return;
        };

        // Hot path: walk the packed lowercased arena directly. No trait
        // dispatch, no string construction, one bounds check per entry.
        let finder = memchr::memmem::Finder::new(needle);
        let n = self.entries.len();
        for i in 0..n {
            if i & 0x3FFF == 0 && cancel.load(AtomicOrdering::Relaxed) {
                return;
            }
            let e = &self.entries[i];
            if e.flags & (FLAG_DELETED | FLAG_ROOT) != 0 {
                continue;
            }
            if let Some(dir_only) = query.only {
                if e.is_dir() != dir_only {
                    continue;
                }
            }
            let start = e.off as usize;
            let hay = &self.lowers[start..start + e.len as usize];
            if finder.find(hay).is_some() {
                out.push(i as Id);
            }
        }
    }

    fn finish(&self, ids: &mut Vec<Id>, sort: SortSpec, limit: usize) -> bool {
        if sort.needs_stats() && !ids.is_empty() {
            // Populate metadata for the rows we will actually order. Beyond
            // this budget the comparator falls back to "unknown sorts last",
            // which keeps very large result sets responsive.
            let budget = ids.len().min(200_000);
            for &id in &ids[..budget] {
                self.ensure_stat(id);
            }
        }
        // Entries are physically stored in name order, so the default view
        // needs no sort at all.
        let presorted = self.name_sorted && sort.key == SortKey::Name && !sort.desc;
        if !presorted {
            sort_ids(self, ids, sort);
        }
        if ids.len() > limit {
            ids.truncate(limit);
        }
        sort.needs_stats()
            && ids
                .iter()
                .any(|&id| self.cached_stat(id).is_none() && !self.is_dir(id))
    }

    pub fn eval(&self, node: &Node, id: Id) -> bool {
        match node {
            Node::True => true,
            Node::False => false,
            Node::And(items) => items.iter().all(|n| self.eval(n, id)),
            Node::Or(items) => items.iter().any(|n| self.eval(n, id)),
            Node::Not(inner) => !self.eval(inner, id),
            Node::Leaf(leaf) => self.eval_leaf(leaf, id),
        }
    }

    fn eval_leaf(&self, leaf: &Leaf, id: Id) -> bool {
        match leaf {
            Leaf::Name(m) => m.matches(self, id),
            Leaf::Ext(list) => {
                let ext = self.ext(id);
                !ext.is_empty() && list.iter().any(|e| e.eq_ignore_ascii_case(ext))
            }
            Leaf::Size(cmp) => {
                if self.is_dir(id) {
                    return false;
                }
                let size = self
                    .ensure_stat(id)
                    .map(|s| s.size)
                    .unwrap_or(u64::MAX);
                cmp.test(size)
            }
            Leaf::Date(field, range) => {
                let value = self.date_value(id, *field);
                match range {
                    DateRange::Unknown => value.is_none(),
                    DateRange::Between(lo, hi) => match value {
                        Some(v) => v >= *lo && v <= *hi,
                        None => false,
                    },
                }
            }
            Leaf::Attrib { must, must_not } => {
                let a = self.attrs(id);
                (a & must) == *must && (a & must_not) == 0
            }
            Leaf::NameLen(cmp) => {
                let len = self.name(id).chars().count() as u32;
                cmp.test(len)
            }
            Leaf::ParentPath { prefix, direct } => {
                let Some(parent) = parent_path(self.path(id), self.name(id)) else {
                    return false;
                };
                if parent.is_empty() {
                    return false;
                }
                if *direct {
                    crate::pattern::eq_ci(parent.as_bytes(), prefix.as_bytes())
                } else {
                    crate::pattern::starts_with_ci(parent.as_bytes(), prefix.as_bytes())
                        && (parent.len() == prefix.len()
                            || parent.as_bytes().get(prefix.len()) == Some(&b'\\'))
                }
            }
            Leaf::Depth(cmp) => cmp.test(self.depth(id)),
            Leaf::Root => self.depth(id) == 0 && !self.entry(id).is_root(),
            Leaf::Frn(n) => self.entry(id).frn == *n,
            Leaf::EmptyDir => {
                self.is_dir(id) && self.child_counts().total[id as usize] == 0
            }
            Leaf::Type(class) => match class {
                TypeClass::Folder => self.is_dir(id),
                TypeClass::File => !self.is_dir(id),
                other => {
                    let ext = self.ext(id);
                    type_ext_list(*other)
                        .iter()
                        .any(|e| e.eq_ignore_ascii_case(ext))
                }
            },
        }
    }

    fn date_value(&self, id: Id, field: DateField) -> Option<i64> {
        let needs_full = matches!(field, DateField::Created | DateField::Accessed);
        let stat = if needs_full {
            self.ensure_full_stat(id)
        } else {
            self.ensure_stat(id)
        }?;
        let v = match field {
            DateField::Modified => stat.mtime,
            DateField::Created => stat.ctime,
            DateField::Accessed => stat.atime,
            DateField::RecentChange => stat.mtime,
        };
        if v <= 0 { None } else { Some(v) }
    }

    fn apply_dupe(&self, ids: &mut Vec<Id>, kind: DupeKind) {
        if ids.len() < 2 {
            return;
        }
        match kind {
            DupeKind::Name => ids.sort_unstable_by(|&a, &b| {
                self.lower_name(a)
                    .cmp(self.lower_name(b))
                    .then_with(|| a.cmp(&b))
            }),
            DupeKind::Size => {
                for &id in ids.iter() {
                    self.ensure_stat(id);
                }
                ids.sort_unstable_by_key(|&id| self.cached_stat(id).map(|s| s.size).unwrap_or(u64::MAX));
            }
            DupeKind::DateModified => {
                for &id in ids.iter() {
                    self.ensure_stat(id);
                }
                ids.sort_unstable_by_key(|&id| self.cached_stat(id).map(|s| s.mtime).unwrap_or(0));
            }
            DupeKind::DateCreated => {
                for &id in ids.iter() {
                    self.ensure_full_stat(id);
                }
                ids.sort_unstable_by_key(|&id| self.cached_stat(id).map(|s| s.ctime).unwrap_or(0));
            }
            DupeKind::DateAccessed => {
                for &id in ids.iter() {
                    self.ensure_full_stat(id);
                }
                ids.sort_unstable_by_key(|&id| self.cached_stat(id).map(|s| s.atime).unwrap_or(0));
            }
        }
        let mut keep = Vec::with_capacity(ids.len());
        let mut i = 0usize;
        while i < ids.len() {
            let mut j = i + 1;
            while j < ids.len() && self.same_dupe_key(ids[i], ids[j], kind) {
                j += 1;
            }
            if j - i > 1 {
                keep.extend_from_slice(&ids[i..j]);
            }
            i = j;
        }
        *ids = keep;
    }

    fn same_dupe_key(&self, a: Id, b: Id, kind: DupeKind) -> bool {
        match kind {
            DupeKind::Name => self.lower_name(a) == self.lower_name(b),
            DupeKind::Size => {
                self.cached_stat(a).map(|s| s.size) == self.cached_stat(b).map(|s| s.size)
            }
            DupeKind::DateModified => {
                self.cached_stat(a).map(|s| s.mtime) == self.cached_stat(b).map(|s| s.mtime)
            }
            DupeKind::DateCreated => {
                self.cached_stat(a).map(|s| s.ctime) == self.cached_stat(b).map(|s| s.ctime)
            }
            DupeKind::DateAccessed => {
                self.cached_stat(a).map(|s| s.atime) == self.cached_stat(b).map(|s| s.atime)
            }
        }
    }
}

fn plain_name_term(root: &Node) -> Option<&NameMatcher> {
    if let Node::Leaf(Leaf::Name(m)) = root {
        if m.is_fast() {
            return Some(m);
        }
    }
    None
}

/// Full path of the containing directory (no trailing separator).
fn parent_path<'a>(path: &'a str, name: &str) -> Option<&'a str> {
    let cut = path.len().checked_sub(name.len())?;
    let mut p = &path[..cut];
    while p.ends_with('\\') && p.len() > 3 {
        p = &p[..p.len() - 1];
    }
    if p.len() == 2 && p.ends_with(':') {
        p = &path[..3];
    }
    Some(p)
}

fn cmp_ci(a: &str, b: &str) -> Ordering {
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    let n = ab.len().min(bb.len());
    for i in 0..n {
        let x = ab[i].to_ascii_lowercase();
        let y = bb[i].to_ascii_lowercase();
        if x != y {
            return x.cmp(&y);
        }
    }
    ab.len().cmp(&bb.len())
}

fn opt_cmp<T: Ord>(a: Option<T>, b: Option<T>, desc: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            if desc {
                y.cmp(&x)
            } else {
                x.cmp(&y)
            }
        }
    }
}

pub fn sort_ids(index: &Index, ids: &mut [Id], spec: SortSpec) {
    match spec.key {
        SortKey::Name => ids.sort_unstable_by(|&a, &b| {
            let o = index.lower_name(a).cmp(index.lower_name(b));
            let o = if spec.desc { o.reverse() } else { o };
            o.then_with(|| cmp_ci(index.path(a), index.path(b)))
        }),
        SortKey::Path => ids.sort_unstable_by(|&a, &b| {
            let o = cmp_ci(index.path(a), index.path(b));
            if spec.desc { o.reverse() } else { o }
        }),
        SortKey::Ext => ids.sort_unstable_by(|&a, &b| {
            let o = cmp_ci(index.ext(a), index.ext(b));
            let o = if spec.desc { o.reverse() } else { o };
            o.then_with(|| index.lower_name(a).cmp(index.lower_name(b)))
        }),
        SortKey::Type => ids.sort_unstable_by(|&a, &b| {
            let o = index
                .is_dir(b)
                .cmp(&index.is_dir(a))
                .then_with(|| cmp_ci(index.ext(a), index.ext(b)));
            let o = if spec.desc { o.reverse() } else { o };
            o.then_with(|| index.lower_name(a).cmp(index.lower_name(b)))
        }),
        SortKey::Size => ids.sort_unstable_by(|&a, &b| {
            let sa = index.cached_stat(a).map(|s| s.size);
            let sb = index.cached_stat(b).map(|s| s.size);
            let o = opt_cmp(sa, sb, spec.desc);
            o.then_with(|| index.lower_name(a).cmp(index.lower_name(b)))
        }),
        SortKey::DateModified | SortKey::DateCreated | SortKey::DateAccessed => {
            ids.sort_unstable_by(|&a, &b| {
                let pick = |id: Id| {
                    index.cached_stat(id).map(|s| match spec.key {
                        SortKey::DateCreated => s.ctime,
                        SortKey::DateAccessed => s.atime,
                        _ => s.mtime,
                    })
                };
                let o = opt_cmp(pick(a), pick(b), spec.desc);
                o.then_with(|| index.lower_name(a).cmp(index.lower_name(b)))
            })
        }
    }
}

pub const ALL_SORTS: &[(SortKey, &str)] = &[
    (SortKey::Name, "Name"),
    (SortKey::Path, "Path"),
    (SortKey::Size, "Size"),
    (SortKey::DateModified, "Date Modified"),
    (SortKey::DateCreated, "Date Created"),
    (SortKey::DateAccessed, "Date Accessed"),
    (SortKey::Ext, "Extension"),
    (SortKey::Type, "Type"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::date;
    use crate::index::{IndexBuilder, RawEntry, VolumeData};

    fn sample() -> Index {
        let raw = |frn, parent, name: &str, dir: bool| {
            RawEntry::new(frn, parent, name, if dir { 0x10 } else { 0x20 })
        };
        let mut b = IndexBuilder::default();
        b.add_volume(VolumeData {
            root: "C:\\".into(),
            root_frn: 5,
            entries: vec![
                raw(10, 5, "src", true),
                raw(11, 10, "main.rs", false),
                raw(12, 10, "lib.rs", false),
                raw(13, 5, "README.md", false),
            ],
        });
        b.build()
    }

    fn run(idx: &Index, text: &str) -> Vec<String> {
        let q = Query::parse(text, &date::local_clock());
        let cancel = AtomicBool::new(false);
        let out = idx.search(&q, SortSpec::default(), 100, &cancel);
        out.ids
            .iter()
            .map(|&id| idx.name(id).to_string())
            .collect()
    }

    #[test]
    fn finds_substrings() {
        let idx = sample();
        assert_eq!(run(&idx, "main"), vec!["main.rs"]);
        assert_eq!(run(&idx, "rs"), vec!["lib.rs", "main.rs"]);
        assert_eq!(run(&idx, "RS"), vec!["lib.rs", "main.rs"]);
    }

    #[test]
    fn ext_and_negation() {
        let idx = sample();
        assert_eq!(run(&idx, "ext:rs"), vec!["lib.rs", "main.rs"]);
        assert_eq!(run(&idx, "ext:rs !main"), vec!["lib.rs"]);
        assert_eq!(run(&idx, "folder:"), vec!["src"]);
    }

    #[test]
    fn paths_and_globs() {
        let idx = sample();
        assert_eq!(run(&idx, "path:src"), vec!["lib.rs", "main.rs", "src"]);
        assert_eq!(run(&idx, "*.md"), vec!["README.md"]);
        assert_eq!(run(&idx, "regex:^m.*\\.rs$"), vec!["main.rs"]);
    }

    #[test]
    fn sorts_by_name() {
        let idx = sample();
        assert_eq!(
            run(&idx, ""),
            vec!["lib.rs", "main.rs", "README.md", "src"]
        );
    }
}
