//! Binary index persistence.
//!
//! Everything keeps its database on disk so a restart is instant instead of a
//! re-index. This is the same idea with a simpler container: the arenas are
//! written verbatim and every offset is validated on load, so a truncated or
//! corrupt file is rejected rather than producing a panic later.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use crate::date;
use crate::index::{Entry, Index, StatStore, ChildCounts};
use std::sync::OnceLock;

const MAGIC: &[u8; 8] = b"EVRIDX02";
const VERSION: u32 = 4;

fn w_u16(w: &mut impl Write, v: u16) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn w_u32(w: &mut impl Write, v: u32) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn w_u64(w: &mut impl Write, v: u64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn w_i64(w: &mut impl Write, v: i64) -> io::Result<()> {
    w.write_all(&v.to_le_bytes())
}
fn w_bytes(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    w_u32(w, b.len() as u32)?;
    w.write_all(b)
}
fn w_str(w: &mut impl Write, s: &str) -> io::Result<()> {
    w_bytes(w, s.as_bytes())
}

fn r_u16(r: &mut impl Read) -> io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}
fn r_u32(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}
fn r_u64(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}
fn r_i64(r: &mut impl Read) -> io::Result<i64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(i64::from_le_bytes(b))
}
fn r_bytes(r: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let len = r_u32(r)? as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "arena too large"));
    }
    let mut v = vec![0u8; len];
    r.read_exact(&mut v)?;
    Ok(v)
}
fn r_str(r: &mut impl Read) -> io::Result<String> {
    let b = r_bytes(r, 1 << 16)?;
    String::from_utf8(b).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "bad utf8"))
}

fn fnv(data: &[u8], seed: u64) -> u64 {
    let mut h = seed;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Index {
    /// Write the index to `path`. The write is atomic: a temporary file is
    /// renamed over the destination only after it is complete.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let tmp = path.with_extension("tmp");
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        {
            let file = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(1 << 20, file);
            self.write_to(&mut w)?;
            w.flush()?;
            w.get_ref().sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        w.write_all(MAGIC)?;
        w_u32(w, VERSION)?;
        w_u64(w, self.build_ms)?;
        w_u64(w, self.built_unix)?;
        w_u32(w, self.name_sorted as u32)?;
        w_u32(w, self.sources.len() as u32)?;
        for s in &self.sources {
            w_str(w, s)?;
        }
        w_u32(w, self.journal.len() as u32)?;
        for &(letter, id, usn) in &self.journal {
            w_u32(w, letter as u32)?;
            w_u64(w, id)?;
            w_i64(w, usn)?;
        }
        w_u32(w, self.roots.len() as u32)?;
        for &r in &self.roots {
            w_u32(w, r)?;
        }
        w_u32(w, self.entries.len() as u32)?;
        for (i, e) in self.entries.iter().enumerate() {
            w_u32(w, e.off)?;
            w_u16(w, e.len)?;
            w_u16(w, e.flags)?;
            w_u32(w, e.parent)?;
            w_u32(w, e.path_off)?;
            w_u32(w, e.path_len)?;
            w_u64(w, e.frn)?;
            w_u32(w, e.attrs)?;
            let stat = self.raw_stat(i);
            w_u64(w, stat.0)?;
            w_i64(w, stat.1)?;
            w_i64(w, stat.2)?;
            w_i64(w, stat.3)?;
        }
        w_bytes(w, &self.names)?;
        w_bytes(w, &self.lowers)?;
        w_bytes(w, &self.paths)?;
        let mut h = fnv(&self.names, 0xcbf29ce484222325);
        h = fnv(&self.lowers, h);
        h = fnv(&self.paths, h);
        w_u64(w, h)?;
        Ok(())
    }

    pub fn load(path: &Path) -> io::Result<Index> {
        let file = File::open(path)?;
        let mut r = BufReader::with_capacity(1 << 20, file);
        let mut magic = [0u8; 8];
        r.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not an ev index"));
        }
        let version = r_u32(&mut r)?;
        if version != VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "index version"));
        }
        let build_ms = r_u64(&mut r)?;
        let built_unix = r_u64(&mut r)?;
        let name_sorted = r_u32(&mut r)? != 0;
        let n_sources = r_u32(&mut r)? as usize;
        if n_sources > 4096 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad source count"));
        }
        let mut sources = Vec::with_capacity(n_sources);
        for _ in 0..n_sources {
            sources.push(r_str(&mut r)?);
        }
        let n_journal = r_u32(&mut r)? as usize;
        if n_journal > 4096 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad journal count"));
        }
        let mut journal = Vec::with_capacity(n_journal);
        for _ in 0..n_journal {
            let letter = char::from_u32(r_u32(&mut r)?).unwrap_or('?');
            let id = r_u64(&mut r)?;
            let usn = r_i64(&mut r)?;
            journal.push((letter, id, usn));
        }
        let n_roots = r_u32(&mut r)? as usize;
        let mut roots = Vec::with_capacity(n_roots);
        for _ in 0..n_roots {
            roots.push(r_u32(&mut r)?);
        }
        let n = r_u32(&mut r)? as usize;
        if n > 40_000_000 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "index too large"));
        }
        let mut entries = Vec::with_capacity(n);
        let mut stats: Vec<(u64, i64, i64, i64)> = Vec::with_capacity(n);
        for _ in 0..n {
            let off = r_u32(&mut r)?;
            let len = r_u16(&mut r)?;
            let flags = r_u16(&mut r)?;
            let parent = r_u32(&mut r)?;
            let path_off = r_u32(&mut r)?;
            let path_len = r_u32(&mut r)?;
            let frn = r_u64(&mut r)?;
            let attrs = r_u32(&mut r)?;
            let s0 = r_u64(&mut r)?;
            let s1 = r_i64(&mut r)?;
            let s2 = r_i64(&mut r)?;
            let s3 = r_i64(&mut r)?;
            entries.push(Entry {
                off,
                len,
                flags,
                parent,
                path_off,
                path_len,
                frn,
                attrs,
            });
            stats.push((s0, s1, s2, s3));
        }
        let names = r_bytes(&mut r, 1 << 30)?;
        let lowers = r_bytes(&mut r, 1 << 30)?;
        let paths = r_bytes(&mut r, 1 << 30)?;
        let expect = r_u64(&mut r)?;
        let mut h = fnv(&names, 0xcbf29ce484222325);
        h = fnv(&lowers, h);
        h = fnv(&paths, h);
        if h != expect {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "checksum mismatch"));
        }

        // Structural validation: every offset must be inside its arena and
        // names/lowers must stay in lockstep.
        if lowers.len() != names.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "arena mismatch"));
        }
        for (i, e) in entries.iter().enumerate() {
            let name_end = e.off as usize + e.len as usize;
            let path_end = e.path_off as usize + e.path_len as usize;
            if name_end > names.len() || path_end > paths.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("entry {i} out of bounds"),
                ));
            }
            if e.parent != u32::MAX && e.parent as usize >= n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("entry {i} bad parent"),
                ));
            }
        }
        for &r in &roots {
            if r as usize >= n {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "bad root"));
            }
        }

        let mut by_frn: Vec<(u64, u32)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.frn, i as u32))
            .collect();
        by_frn.sort_unstable_by_key(|&(f, _)| f);
        by_frn.dedup_by_key(|&mut (f, _)| f);

        let stat = StatStore::new(n);
        let index = Index {
            entries,
            names,
            lowers,
            paths,
            roots,
            by_frn,
            stat,
            child_counts: OnceLock::<ChildCounts>::new(),
            sources,
            build_ms,
            built_unix,
            name_sorted,
            clock: date::local_clock(),
            journal,
        };
        for (i, (s0, s1, s2, s3)) in stats.into_iter().enumerate() {
            index.set_raw_stat(i, s0, s1, s2, s3);
        }
        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{IndexBuilder, RawEntry, VolumeData};

    #[test]
    fn roundtrip() {
        let mut b = IndexBuilder::default();
        b.add_volume(VolumeData {
            root: "C:\\".into(),
            root_frn: 5,
            entries: vec![
                RawEntry::new(10, 5, "folder", 0x10),
                RawEntry::new(11, 10, "a.txt", 0x20).with_stat(42, 133_000_000_000_000_000, 0, 0),
            ],
        });
        let idx = b.build();
        let path = std::env::temp_dir().join("everything-rs-roundtrip.evdb");
        idx.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        assert_eq!(back.len(), idx.len());
        let id = back.find_frn(11).unwrap();
        assert_eq!(back.path(id), "C:\\folder\\a.txt");
        assert_eq!(back.name(id), "a.txt");
        assert_eq!(back.cached_stat(id).unwrap().size, 42);
        assert_eq!(back.sources, idx.sources);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_corrupt_files() {
        let path = std::env::temp_dir().join("everything-rs-corrupt.evdb");
        std::fs::write(&path, b"not an index").unwrap();
        assert!(Index::load(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
