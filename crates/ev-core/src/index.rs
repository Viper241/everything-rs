//! The in-memory index.
//!
//! Layout mirrors what makes Everything fast: one compact record per file
//! system object, all strings packed into flat arenas, and a lowercased mirror
//! of every name so the hot search loop never has to case-fold.
//!
//! ```text
//! Entry (32 bytes)      names arena        lowers arena        paths arena
//! +----------------+    +-----------+      +-----------+       +----------------+
//! | off  : u32     | -> | "README"  |      | "readme"  |       | "C:\src\..."   |
//! | len  : u16     |    | "main.rs" |      | "main.rs" |       | "C:\src\..."   |
//! | flags: u16     |    +-----------+      +-----------+       +----------------+
//! | parent: u32    | -> index of the parent directory
//! | path_off/len   | -> full path (includes the name)
//! | frn, attrs     |
//! +----------------+
//! ```

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use crate::date::{self, LocalClock};

pub type Id = u32;
pub const NONE: Id = u32::MAX;

pub const FLAG_DIR: u16 = 1 << 0;
pub const FLAG_ROOT: u16 = 1 << 1;
pub const FLAG_DELETED: u16 = 1 << 2;

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Entry {
    pub off: u32,
    pub len: u16,
    pub flags: u16,
    pub parent: Id,
    pub path_off: u32,
    pub path_len: u32,
    pub frn: u64,
    pub attrs: u32,
}

impl Entry {
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.flags & FLAG_DIR != 0
    }
    #[inline]
    pub fn is_root(&self) -> bool {
        self.flags & FLAG_ROOT != 0
    }
    #[inline]
    pub fn is_deleted(&self) -> bool {
        self.flags & FLAG_DELETED != 0
    }
}

/// One record handed to the builder by a volume scanner.
///
/// `size`/`mtime`/`ctime`/`atime` may be zero when the scanner did not read
/// metadata (the MFT path); the index then fills them lazily.
#[derive(Clone, Debug, Default)]
pub struct RawEntry {
    pub frn: u64,
    pub parent: u64,
    pub name: String,
    pub attrs: u32,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
}

impl RawEntry {
    pub fn new(frn: u64, parent: u64, name: impl Into<String>, attrs: u32) -> Self {
        Self {
            frn,
            parent,
            name: name.into(),
            attrs,
            ..Default::default()
        }
    }
    pub fn with_stat(mut self, size: u64, mtime: i64, ctime: i64, atime: i64) -> Self {
        self.size = size;
        self.mtime = mtime;
        self.ctime = ctime;
        self.atime = atime;
        self
    }
    #[inline]
    pub fn has_stat(&self) -> bool {
        self.mtime != 0
    }
}

#[derive(Clone, Debug)]
pub struct VolumeData {
    /// e.g. `C:\` - must end with a separator.
    pub root: String,
    /// FRN of the volume root directory (5 on NTFS).
    pub root_frn: u64,
    pub entries: Vec<RawEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
}

const UNKNOWN: u64 = u64::MAX;
const FAILED: u64 = u64::MAX - 1;
const UNKNOWN_DATE: i64 = i64::MIN;

pub(crate) struct StatStore {
    size: Vec<AtomicU64>,
    mtime: Vec<AtomicI64>,
    ctime: Vec<AtomicI64>,
    atime: Vec<AtomicI64>,
}

impl StatStore {
    pub(crate) fn new(n: usize) -> Self {
        let mut size = Vec::with_capacity(n);
        let mut mtime = Vec::with_capacity(n);
        let mut ctime = Vec::with_capacity(n);
        let mut atime = Vec::with_capacity(n);
        for _ in 0..n {
            size.push(AtomicU64::new(UNKNOWN));
            mtime.push(AtomicI64::new(UNKNOWN_DATE));
            ctime.push(AtomicI64::new(UNKNOWN_DATE));
            atime.push(AtomicI64::new(UNKNOWN_DATE));
        }
        Self {
            size,
            mtime,
            ctime,
            atime,
        }
    }
}

#[derive(Default)]
pub struct ChildCounts {
    pub total: Vec<u32>,
    pub files: Vec<u32>,
    pub folders: Vec<u32>,
}

pub struct Index {
    pub(crate) entries: Vec<Entry>,
    pub(crate) names: Vec<u8>,
    pub(crate) lowers: Vec<u8>,
    pub(crate) paths: Vec<u8>,
    pub(crate) roots: Vec<Id>,
    /// Sorted by FRN for O(log n) lookups during journal updates.
    pub(crate) by_frn: Vec<(u64, Id)>,
    pub(crate) stat: StatStore,
    pub(crate) child_counts: OnceLock<ChildCounts>,
    pub sources: Vec<String>,
    pub build_ms: u64,
    /// Unix seconds when the index was built (used for cache freshness).
    pub built_unix: u64,
    pub clock: LocalClock,
    /// True while `entries` is physically ordered by lowercased name, which
    /// makes the default "Name ascending" view a zero-cost scan.
    pub name_sorted: bool,
    /// `(volume, journal id, next usn)` captured when the index was written,
    /// used to replay changes that happened while the app was closed.
    pub journal: Vec<(char, u64, i64)>,
}

impl Index {
    pub fn builder() -> IndexBuilder {
        IndexBuilder::default()
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn roots(&self) -> &[Id] {
        &self.roots
    }

    #[inline]
    pub fn entry(&self, id: Id) -> &Entry {
        &self.entries[id as usize]
    }

    #[inline]
    pub fn name(&self, id: Id) -> &str {
        let e = &self.entries[id as usize];
        // SAFETY: every string pushed into the arena came from a `&str`.
        unsafe {
            std::str::from_utf8_unchecked(&self.names[e.off as usize..(e.off as usize + e.len as usize)])
        }
    }

    #[inline]
    pub fn lower_name(&self, id: Id) -> &str {
        let e = &self.entries[id as usize];
        unsafe {
            std::str::from_utf8_unchecked(
                &self.lowers[e.off as usize..(e.off as usize + e.len as usize)],
            )
        }
    }

    #[inline]
    pub fn path(&self, id: Id) -> &str {
        let e = &self.entries[id as usize];
        unsafe {
            std::str::from_utf8_unchecked(
                &self.paths[e.path_off as usize..(e.path_off as usize + e.path_len as usize)],
            )
        }
    }

    #[inline]
    pub fn is_dir(&self, id: Id) -> bool {
        self.entries[id as usize].is_dir()
    }

    #[inline]
    pub fn attrs(&self, id: Id) -> u32 {
        self.entries[id as usize].attrs
    }

    pub fn depth(&self, id: Id) -> u32 {
        let mut d = 0;
        let mut cur = id;
        while cur != NONE && !self.entries[cur as usize].is_root() && d < 1024 {
            cur = self.entries[cur as usize].parent;
            d += 1;
        }
        d
    }

    pub fn ext(&self, id: Id) -> &str {
        let name = self.name(id);
        match name.rfind('.') {
            Some(i) if i + 1 < name.len() => &name[i + 1..],
            _ => "",
        }
    }

    pub fn file_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| !e.is_dir() && !e.is_deleted())
            .count()
    }

    pub fn dir_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.is_dir() && !e.is_deleted())
            .count()
    }

    pub fn child_counts(&self) -> &ChildCounts {
        self.child_counts.get_or_init(|| {
            let n = self.entries.len();
            let mut c = ChildCounts {
                total: vec![0; n],
                files: vec![0; n],
                folders: vec![0; n],
            };
            for e in &self.entries {
                if e.is_deleted() || e.parent == NONE {
                    continue;
                }
                let p = e.parent as usize;
                if p >= n {
                    continue;
                }
                c.total[p] += 1;
                if e.is_dir() {
                    c.folders[p] += 1;
                } else {
                    c.files[p] += 1;
                }
            }
            c
        })
    }

    pub fn find_frn(&self, frn: u64) -> Option<Id> {
        self.by_frn
            .binary_search_by_key(&frn, |&(f, _)| f)
            .ok()
            .map(|i| self.by_frn[i].1)
    }

    // ---------------------------------------------------------------- stats

    /// Cached metadata only; does not touch the file system.
    pub fn cached_stat(&self, id: Id) -> Option<Stat> {
        let i = id as usize;
        let s = self.stat.size[i].load(Ordering::Relaxed);
        if s == UNKNOWN || s == FAILED {
            return None;
        }
        Some(Stat {
            size: s,
            mtime: self.stat.mtime[i].load(Ordering::Relaxed),
            ctime: self.stat.ctime[i].load(Ordering::Relaxed),
            atime: self.stat.atime[i].load(Ordering::Relaxed),
        })
    }

    /// Reads size/modification time from the volume and caches them.
    pub fn ensure_stat(&self, id: Id) -> Option<Stat> {
        if let Some(s) = self.cached_stat(id) {
            return Some(s);
        }
        let i = id as usize;
        if self.stat.size[i].load(Ordering::Relaxed) == FAILED {
            return None;
        }
        let path = self.path(id);
        match stat_path(path) {
            Some(s) => {
                self.store_stat(id, s);
                Some(s)
            }
            None => {
                self.stat.size[i].store(FAILED, Ordering::Relaxed);
                None
            }
        }
    }

    /// Fills all four timestamps (needed by `da:`/`dc:` filters).
    pub fn ensure_full_stat(&self, id: Id) -> Option<Stat> {
        if let Some(s) = self.cached_stat(id) {
            if s.ctime != UNKNOWN_DATE && s.atime != UNKNOWN_DATE {
                return Some(s);
            }
        }
        let path = self.path(id);
        let s = stat_path(path)?;
        self.store_stat(id, s);
        Some(s)
    }

    pub fn store_stat(&self, id: Id, s: Stat) {
        let i = id as usize;
        self.stat.size[i].store(s.size, Ordering::Relaxed);
        self.stat.mtime[i].store(s.mtime, Ordering::Relaxed);
        self.stat.ctime[i].store(s.ctime, Ordering::Relaxed);
        self.stat.atime[i].store(s.atime, Ordering::Relaxed);
    }

    pub(crate) fn raw_stat(&self, i: usize) -> (u64, i64, i64, i64) {
        (
            self.stat.size[i].load(Ordering::Relaxed),
            self.stat.mtime[i].load(Ordering::Relaxed),
            self.stat.ctime[i].load(Ordering::Relaxed),
            self.stat.atime[i].load(Ordering::Relaxed),
        )
    }

    pub(crate) fn set_raw_stat(&self, i: usize, size: u64, mtime: i64, ctime: i64, atime: i64) {
        self.stat.size[i].store(size, Ordering::Relaxed);
        self.stat.mtime[i].store(mtime, Ordering::Relaxed);
        self.stat.ctime[i].store(ctime, Ordering::Relaxed);
        self.stat.atime[i].store(atime, Ordering::Relaxed);
    }

    /// Total resident bytes, for the status bar.
    pub fn memory_bytes(&self) -> usize {
        self.names.len()
            + self.lowers.len()
            + self.paths.len()
            + self.entries.len() * std::mem::size_of::<Entry>()
            + self.by_frn.len() * std::mem::size_of::<(u64, Id)>()
            + self.entries.len() * std::mem::size_of::<AtomicU64>() * 4
    }

    pub fn refreshed_clock(&mut self) {
        self.clock = date::local_clock();
    }

    // --------------------------------------------------------------- deltas

    /// Apply a batch of USN journal deltas.
    ///
    /// Returns true when directory structure changed and callers should treat
    /// cached search results as stale.
    pub fn apply_delta(&mut self, upserts: &[RawEntry], removes: &[u64]) -> bool {
        let mut structural = false;

        for &frn in removes {
            if let Some(id) = self.find_frn(frn) {
                self.entries[id as usize].flags |= FLAG_DELETED;
            }
        }

        for e in upserts {
            if e.name.is_empty() {
                continue;
            }
            if let Some(id) = self.find_frn(e.frn) {
                let cur = self.entries[id as usize];
                let same_name = self.name(id) == e.name;
                if !same_name {
                    self.name_sorted = false;
                }
                let parent = self
                    .find_frn(e.parent)
                    .unwrap_or_else(|| self.volume_root_of(id));
                if !same_name || cur.parent != parent {
                    structural = true;
                    if !same_name {
                        let (off, len) = self.push_name(&e.name);
                        let ent = &mut self.entries[id as usize];
                        ent.off = off;
                        ent.len = len;
                    }
                    self.entries[id as usize].parent = parent;
                }
                self.entries[id as usize].attrs = e.attrs;
                self.entries[id as usize].flags &= !FLAG_DELETED;
                self.store_stat(
                    id,
                    Stat {
                        size: UNKNOWN,
                        mtime: UNKNOWN_DATE,
                        ctime: UNKNOWN_DATE,
                        atime: UNKNOWN_DATE,
                    },
                );
            } else {
                let parent = match self.find_frn(e.parent) {
                    Some(p) => p,
                    None => continue,
                };
                let id = self.entries.len() as Id;
                let (off, len) = self.push_name(&e.name);
                let mut flags = 0;
                if e.attrs & 0x10 != 0 {
                    flags |= FLAG_DIR;
                }
                let path = self.compose_path(parent, &e.name);
                let pending = Stat {
                    size: UNKNOWN,
                    mtime: UNKNOWN_DATE,
                    ctime: UNKNOWN_DATE,
                    atime: UNKNOWN_DATE,
                };
                self.entries.push(Entry {
                    off,
                    len,
                    flags,
                    parent,
                    path_off: path.0,
                    path_len: path.1,
                    frn: e.frn,
                    attrs: e.attrs,
                });
                self.by_frn.push((e.frn, id));
                self.stat.push();
                self.store_stat(id, pending);
                self.name_sorted = false;
            }
            // Keep the sorted FRN map compact: entries are appended, then the
            // whole vector is re-sorted at the end of the batch.
        }

        if !upserts.is_empty() {
            self.by_frn.sort_unstable_by_key(|&(f, _)| f);
            self.by_frn.dedup_by_key(|&mut (f, _)| f);
        }

        if structural {
            self.rebuild_paths();
        }
        structural
    }

    fn volume_root_of(&self, id: Id) -> Id {
        let mut cur = id;
        let mut guard = 0;
        while !self.entries[cur as usize].is_root() && guard < 1024 {
            let p = self.entries[cur as usize].parent;
            if p == NONE || p == cur {
                break;
            }
            cur = p;
            guard += 1;
        }
        if self.entries[cur as usize].is_root() {
            cur
        } else {
            self.roots.first().copied().unwrap_or(cur)
        }
    }

    fn push_name(&mut self, name: &str) -> (u32, u16) {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        self.lowers
            .extend(name.bytes().map(|b| b.to_ascii_lowercase()));
        (off, name.len() as u16)
    }

    fn compose_path(&mut self, parent: Id, name: &str) -> (u32, u32) {
        let start = self.paths.len() as u32;
        let pe = self.entries[parent as usize];
        if pe.path_len > 0 {
            self.paths
                .extend_from_within(pe.path_off as usize..(pe.path_off + pe.path_len) as usize);
            if !pe.is_root() {
                self.paths.push(b'\\');
            }
        }
        self.paths.extend_from_slice(name.as_bytes());
        (start, self.paths.len() as u32 - start)
    }

    /// Reorder entries by lowercased name (stable across runs) so the default
    /// result order needs no sort. Returns the old-id -> new-id permutation.
    fn sort_entries_by_name(&mut self) -> Vec<u32> {
        let n = self.entries.len();
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let ord = self.lower_name(a).cmp(self.lower_name(b));
            if ord == std::cmp::Ordering::Equal {
                a.cmp(&b)
            } else {
                ord
            }
        });
        let mut pos = vec![0u32; n];
        for (new, &old) in order.iter().enumerate() {
            pos[old as usize] = new as u32;
        }
        let mut new_entries = Vec::with_capacity(n);
        for &old in &order {
            new_entries.push(self.entries[old as usize]);
        }
        for e in &mut new_entries {
            if e.parent != NONE {
                e.parent = pos[e.parent as usize];
            }
        }
        for r in &mut self.roots {
            *r = pos[*r as usize];
        }
        for (_, id) in &mut self.by_frn {
            *id = pos[*id as usize];
        }
        self.entries = new_entries;
        self.name_sorted = true;
        pos
    }

    fn rebuild_paths(&mut self) {
        self.paths.clear();
        let n = self.entries.len();
        // 0 = pending, 1 = on the current walk, 2 = done
        let mut state = vec![0u8; n];
        for i in 0..self.roots.len() {
            let r = self.roots[i];
            let e = self.entries[r as usize];
            let start = self.paths.len() as u32;
            self.paths
                .extend_from_slice(&self.names[e.off as usize..(e.off as usize + e.len as usize)]);
            let ent = &mut self.entries[r as usize];
            ent.path_off = start;
            ent.path_len = self.paths.len() as u32 - start;
            state[r as usize] = 2;
        }
        let mut chain: Vec<Id> = Vec::with_capacity(64);
        for start in 0..n {
            if state[start] != 0 {
                continue;
            }
            chain.clear();
            let mut cur = start as Id;
            let mut guard = 0usize;
            loop {
                match state[cur as usize] {
                    2 => break,
                    1 => {
                        // Cycle (corrupt MFT): detach from the cycle.
                        let root = self.volume_root_of(cur);
                        self.entries[cur as usize].parent = root;
                        break;
                    }
                    _ => {}
                }
                state[cur as usize] = 1;
                chain.push(cur);
                guard += 1;
                if guard > 512 {
                    let root = self.volume_root_of(cur);
                    self.entries[cur as usize].parent = root;
                    break;
                }
                let p = self.entries[cur as usize].parent;
                if p == NONE || p == cur {
                    break;
                }
                cur = p;
            }
            for i in (0..chain.len()).rev() {
                let id = chain[i];
                let parent = self.entries[id as usize].parent;
                let (poff, plen) = if parent != NONE && (parent as usize) < n {
                    let pe = self.entries[parent as usize];
                    (pe.path_off, pe.path_len)
                } else {
                    (0, 0)
                };
                let path_start = self.paths.len() as u32;
                let ent = self.entries[id as usize];
                if plen > 0 {
                    self.paths
                        .extend_from_within(poff as usize..(poff + plen) as usize);
                    if parent != NONE && !self.entries[parent as usize].is_root() {
                        self.paths.push(b'\\');
                    }
                }
                self.paths
                    .extend_from_slice(&self.names[ent.off as usize..(ent.off as usize + ent.len as usize)]);
                let len = self.paths.len() as u32 - path_start;
                let e = &mut self.entries[id as usize];
                e.path_off = path_start;
                e.path_len = len;
                state[id as usize] = 2;
            }
        }
    }
}

impl StatStore {
    fn push(&mut self) {
        self.size.push(AtomicU64::new(UNKNOWN));
        self.mtime.push(AtomicI64::new(UNKNOWN_DATE));
        self.ctime.push(AtomicI64::new(UNKNOWN_DATE));
        self.atime.push(AtomicI64::new(UNKNOWN_DATE));
    }
}

#[derive(Default)]
pub struct IndexBuilder {
    volumes: Vec<VolumeData>,
    sources: Vec<String>,
    build_ms: u64,
}

impl IndexBuilder {
    pub fn add_volume(&mut self, v: VolumeData) {
        self.sources.push(v.root.clone());
        self.volumes.push(v);
    }

    pub fn set_build_ms(&mut self, ms: u64) {
        self.build_ms = ms;
    }

    pub fn build(self) -> Index {
        let total: usize = self.volumes.iter().map(|v| v.entries.len() + 1).sum();
        let mut entries = Vec::with_capacity(total);
        let mut names = Vec::with_capacity(total * 24);
        let mut lowers = Vec::with_capacity(total * 24);
        let mut paths = Vec::with_capacity(total * 64);
        let mut roots = Vec::with_capacity(self.volumes.len());
        let mut by_frn: Vec<(u64, Id)> = Vec::with_capacity(total);
        let mut stats: Vec<(Id, Stat)> = Vec::new();
        let mut map: HashMap<u64, Id> = HashMap::with_capacity(total.min(1 << 20));

        for vol in &self.volumes {
            let root = if vol.root.ends_with('\\') {
                vol.root.clone()
            } else {
                format!("{}\\", vol.root)
            };
            let root_id = entries.len() as Id;
            let off = names.len() as u32;
            names.extend_from_slice(root.as_bytes());
            lowers.extend(root.bytes().map(|b| b.to_ascii_lowercase()));
            let path_off = paths.len() as u32;
            paths.extend_from_slice(root.as_bytes());
            entries.push(Entry {
                off,
                len: root.len() as u16,
                flags: FLAG_DIR | FLAG_ROOT,
                parent: NONE,
                path_off,
                path_len: root.len() as u32,
                frn: vol.root_frn,
                attrs: 0x10,
            });
            roots.push(root_id);
            map.insert(vol.root_frn, root_id);
            by_frn.push((vol.root_frn, root_id));

            let base = entries.len() as Id;
            for (i, e) in vol.entries.iter().enumerate() {
                if e.name.is_empty() {
                    continue;
                }
                let id = base + i as Id;
                let off = names.len() as u32;
                names.extend_from_slice(e.name.as_bytes());
                lowers.extend(e.name.bytes().map(|b| b.to_ascii_lowercase()));
                let mut flags = 0;
                if e.attrs & 0x10 != 0 {
                    flags |= FLAG_DIR;
                }
                entries.push(Entry {
                    off,
                    len: e.name.len() as u16,
                    flags,
                    parent: root_id,
                    path_off: 0,
                    path_len: 0,
                    frn: e.frn,
                    attrs: e.attrs,
                });
                if e.has_stat() {
                    stats.push((
                        id,
                        Stat {
                            size: e.size,
                            mtime: e.mtime,
                            ctime: e.ctime,
                            atime: e.atime,
                        },
                    ));
                }
                map.entry(e.frn).or_insert(id);
                by_frn.push((e.frn, id));
            }

            // Second pass: resolve parents now that every FRN is known.
            for (i, e) in vol.entries.iter().enumerate() {
                if e.name.is_empty() {
                    continue;
                }
                let id = base + i as Id;
                let parent = map.get(&e.parent).copied().unwrap_or(root_id);
                entries[id as usize].parent = if parent == id { root_id } else { parent };
            }
        }

        by_frn.sort_unstable_by_key(|&(f, _)| f);
        by_frn.dedup_by_key(|&mut (f, _)| f);

        let n = entries.len();
        let stat = StatStore::new(n);
        let mut index = Index {
            entries,
            names,
            lowers,
            paths,
            roots,
            by_frn,
            stat,
            child_counts: OnceLock::new(),
            sources: self.sources,
            build_ms: self.build_ms,
            built_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            clock: date::local_clock(),
            name_sorted: false,
            journal: Vec::new(),
        };
        index.rebuild_paths();
        let pos = index.sort_entries_by_name();
        for (id, stat) in stats {
            index.store_stat(pos[id as usize], stat);
        }
        index
    }
}

#[cfg(windows)]
pub fn stat_path(path: &str) -> Option<Stat> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileAttributesExW, GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide.as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut core::ffi::c_void,
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |f: FILETIME| -> i64 {
        ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as i64
    };
    let size = if data.dwFileAttributes & 0x10 != 0 {
        0
    } else {
        ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64
    };
    Some(Stat {
        size,
        mtime: ticks(data.ftLastWriteTime),
        ctime: ticks(data.ftCreationTime),
        atime: ticks(data.ftLastAccessTime),
    })
}

#[cfg(not(windows))]
pub fn stat_path(_path: &str) -> Option<Stat> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(frn: u64, parent: u64, name: &str, dir: bool) -> RawEntry {
        RawEntry::new(frn, parent, name, if dir { 0x10 } else { 0x20 })
    }

    #[test]
    fn builds_paths() {
        let mut b = IndexBuilder::default();
        b.add_volume(VolumeData {
            root: "C:\\".into(),
            root_frn: 5,
            entries: vec![
                raw(10, 5, "Users", true),
                raw(11, 10, "alice", true),
                raw(12, 11, "notes.txt", false),
            ],
        });
        let idx = b.build();
        assert_eq!(idx.path(idx.find_frn(12).unwrap()), "C:\\Users\\alice\\notes.txt");
        assert_eq!(idx.depth(idx.find_frn(12).unwrap()), 3);
        assert_eq!(idx.name(idx.roots()[0]), "C:\\");
    }

    #[test]
    fn handles_orphans() {
        let mut b = IndexBuilder::default();
        b.add_volume(VolumeData {
            root: "C:\\".into(),
            root_frn: 5,
            entries: vec![raw(99, 12345, "orphan.txt", false)],
        });
        let idx = b.build();
        assert_eq!(idx.path(idx.find_frn(99).unwrap()), "C:\\orphan.txt");
    }

    #[test]
    fn applies_journal_deltas() {
        let mut b = IndexBuilder::default();
        b.add_volume(VolumeData {
            root: "C:\\".into(),
            root_frn: 5,
            entries: vec![raw(10, 5, "old", true), raw(11, 10, "inner.txt", false)],
        });
        let mut idx = b.build();

        // Create
        idx.apply_delta(&[RawEntry::new(20, 5, "new.txt", 0x20)], &[]);
        let id = idx.find_frn(20).unwrap();
        assert_eq!(idx.path(id), "C:\\new.txt");

        // Rename a directory: descendants must follow.
        idx.apply_delta(&[RawEntry::new(10, 5, "renamed", 0x10)], &[]);
        assert_eq!(idx.path(idx.find_frn(11).unwrap()), "C:\\renamed\\inner.txt");

        // Delete
        idx.apply_delta(&[], &[20]);
        assert!(idx.entry(idx.find_frn(20).unwrap()).is_deleted());

        // Recreate the same FRN (NTFS reuse) with a different name.
        idx.apply_delta(&[RawEntry::new(20, 5, "other.txt", 0x20)], &[]);
        let id = idx.find_frn(20).unwrap();
        assert!(!idx.entry(id).is_deleted());
        assert_eq!(idx.path(id), "C:\\other.txt");
    }
}
