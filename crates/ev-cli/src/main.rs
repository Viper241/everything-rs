//! `ev` - command line front-end for everything-rs.
//!
//! ```text
//! ev *.rs dm:today              search the index
//! ev --index                    build an index and print statistics
//! ev --volumes C,D --stats foo  restrict volumes and load metadata
//! ev --watch                    follow the USN journal and print realtime changes
//! ```

use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Instant;

use ev_core::index::Index;
use ev_core::{Query, SortKey, SortSpec, format_filetime, format_size, thousands};
use ev_ntfs::{JournalWatcher, ScanReport};

struct Options {
    search: String,
    volumes: Vec<char>,
    folders: Vec<String>,
    count: usize,
    sort: SortSpec,
    json: bool,
    index_only: bool,
    stats: bool,
    watch: bool,
    detail: bool,
    db: Option<std::path::PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            search: String::new(),
            volumes: Vec::new(),
            folders: Vec::new(),
            count: 100,
            sort: SortSpec::default(),
            json: false,
            index_only: false,
            stats: false,
            watch: false,
            detail: false,
            db: None,
        }
    }
}

fn usage() {
    println!(
        "everything-rs {}\n\
         \n\
         USAGE:\n\
         \x20 ev [options] <search>\n\
         \n\
         OPTIONS:\n\
         \x20 -s, --search <text>      search text (same syntax as Everything)\n\
         \x20 -n, --count <n>          maximum results (default 100)\n\
         \x20     --volumes C,D        volumes to index (default: all fixed)\n\
         \x20     --folders <a;b>      index folders instead of volumes\n\
         \x20     --index              build the index and print statistics\n\
         \x20     --stats              preload size/date metadata\n\
         \x20     --watch              follow the USN journal and print changes\n\
         \x20     --detail             show one result per line with metadata\n\
         \x20     --json               machine readable output\n\
         \x20     --db <file>          load/save the index snapshot (instant restarts)\n\
         \x20     --sort <name>        name|path|size|dm|dc|da|ext|type\n\
         \x20     --sort-descending    reverse the sort order\n\
         \x20 -h, --help               show this help\n\
         \n\
         SEARCH SYNTAX (Everything compatible):\n\
         \x20 foo bar        both terms             foo|bar       either term\n\
         \x20 !foo           exclude                \"exact phrase\"\n\
         \x20 *.rs           wildcards              regex:^m.*\\.rs$\n\
         \x20 ext:rs;toml    extensions             size:>1mb\n\
         \x20 dm:today       date modified          dc:/da: created/accessed\n\
         \x20 path:src       match the full path    parent:C:\\Windows\n\
         \x20 folder:/file:  only folders/files     attrib:HD\n\
         \x20 dupe: sizedupe: dmdupe: count:10      audio: video: pic: doc: exe:\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut free: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> Result<String, String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| format!("missing value for {a}"))
        };
        match a {
            "-h" | "--help" => {
                usage();
                std::process::exit(0);
            }
            "-s" | "--search" => o.search = next(&mut i)?,
            "-n" | "--count" => {
                o.count = next(&mut i)?
                    .parse()
                    .map_err(|_| "invalid --count".to_string())?
            }
            "--volumes" => {
                o.volumes = next(&mut i)?
                    .split([',', ';'])
                    .filter_map(|s| s.trim().chars().next())
                    .map(|c| c.to_ascii_uppercase())
                    .collect()
            }
            "--folders" => {
                o.folders = next(&mut i)?
                    .split(';')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
            "--sort" => {
                let name = next(&mut i)?;
                o.sort = SortSpec::parse(&name, o.sort.desc)
                    .ok_or_else(|| format!("unknown sort '{name}'"))?
            }
            "--sort-descending" => o.sort.desc = true,
            "--sort-ascending" => o.sort.desc = false,
            "--index" => o.index_only = true,
            "--stats" => o.stats = true,
            "--watch" => o.watch = true,
            "--detail" => o.detail = true,
            "--db" => {
                o.db = Some(std::path::PathBuf::from(next(&mut i)?));
            }
            "--json" => o.json = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option '{other}' (try --help)"));
            }
            other => free.push(other.to_string()),
        }
        i += 1;
    }
    if o.search.is_empty() {
        o.search = free.join(" ");
    }
    Ok(o)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("ev: {e}");
            std::process::exit(2);
        }
    };

    let volumes = if !opts.volumes.is_empty() {
        opts.volumes.clone()
    } else {
        ev_ntfs::list_volumes()
            .into_iter()
            .filter(|v| v.kind == "fixed")
            .map(|v| v.letter)
            .collect()
    };
    if volumes.is_empty() && opts.folders.is_empty() {
        eprintln!("ev: no volumes found");
        std::process::exit(1);
    }

    let counter = AtomicU64::new(0);
    let t0 = Instant::now();

    // With --db, prefer a saved snapshot: validate it against the USN journal
    // and replay whatever changed while we were not running.
    let mut loaded = None;
    if let Some(db) = &opts.db {
        if let Ok(mut idx) = Index::load(db) {
            if ev_ntfs::cache_usable(&idx, &volumes) {
                let marks = idx.journal.clone();
                if !marks.is_empty() {
                    if marks.len() == volumes.len() && ev_ntfs::catch_up(&mut idx, &marks) {
                        loaded = Some(idx);
                    }
                } else {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if now.saturating_sub(idx.built_unix) < 3600 {
                        loaded = Some(idx);
                    }
                }
            }
        }
    }

    let (mut index, report) = match loaded {
        Some(idx) => (idx, ScanReport::default()),
        None => {
            let (idx, rep) = if opts.folders.is_empty() {
                ev_ntfs::scan_volumes(&volumes, &counter)
            } else {
                ev_ntfs::scan_folders(&opts.folders, &counter)
            };
            (idx, rep)
        }
    };
    let build = t0.elapsed();
    let from_cache = index.journal.is_empty() && opts.db.is_some() && !index.sources.is_empty() && report.entries == 0;
    if let Some(db) = &opts.db {
        if report.entries > 0 || index.journal.is_empty() {
            index.journal = ev_ntfs::journal_marks(&volumes);
            if let Err(e) = index.save(db) {
                eprintln!("ev: could not save index: {e}");
            }
        }
    }
    let _ = from_cache;

    if opts.stats {
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        ev_ntfs::prefetch_stats(&index, ev_ntfs::cpu_count(), &cancel, &progress);
    }

    if opts.index_only {
        print_stats(&index, &report, build);
        if opts.watch {
            watch(&mut index, &volumes);
        }
        return;
    }

    if opts.search.is_empty() && !opts.watch {
        print_stats(&index, &report, build);
        return;
    }

    let clock = ev_core::local_clock();
    let query = Query::parse(&opts.search, &clock);
    let cancel = AtomicBool::new(false);
    let outcome = index.search(&query, opts.sort, opts.count, &cancel);

    for w in &query.warnings {
        eprintln!("ev: warning: {w}");
    }

    if opts.json {
        print_json(&index, &outcome, &query);
    } else {
        if opts.detail {
            println!(
                "{:>10}  {:<16}  {:<5}  PATH",
                "SIZE", "MODIFIED", "TYPE"
            );
        }
        for &id in &outcome.ids {
            if opts.detail {
                let stat = index.cached_stat(id);
                let size = if index.is_dir(id) {
                    "<DIR>".to_string()
                } else {
                    stat.map(|s| format_size(s.size)).unwrap_or_default()
                };
                let date = stat
                    .map(|s| format_filetime(s.mtime, &clock))
                    .unwrap_or_default();
                let kind = if index.is_dir(id) { "dir" } else { "file" };
                println!("{:>10}  {:<16}  {:<5}  {}", size, date, kind, index.path(id));
            } else {
                println!("{}", index.path(id));
            }
        }
        eprintln!(
            "{} of {} objects  ({:.1} ms search, {:.0} ms index)",
            thousands(outcome.ids.len()),
            thousands(outcome.total),
            outcome.elapsed.as_secs_f64() * 1000.0,
            build.as_secs_f64() * 1000.0
        );
    }

    if opts.watch {
        watch(&mut index, &volumes);
    }
}

fn print_stats(index: &Index, report: &ScanReport, build: std::time::Duration) {
    println!("everything-rs {} index", env!("CARGO_PKG_VERSION"));
    println!("  objects        : {}", thousands(index.len()));
    println!("  files          : {}", thousands(index.file_count()));
    println!("  folders        : {}", thousands(index.dir_count()));
    println!(
        "  memory         : {:.1} MB",
        index.memory_bytes() as f64 / (1024.0 * 1024.0)
    );
    println!(
        "  build time     : {:.0} ms ({} entries)",
        build.as_secs_f64() * 1000.0,
        thousands(report.entries as usize)
    );
    for (letter, mode) in &report.modes {
        println!("  volume {letter}:     {mode}");
    }
    for e in &report.errors {
        println!("  note           : {e}");
    }
    println!("  sources        : {}", index.sources.join("  "));
}

fn print_json(index: &Index, outcome: &ev_core::Outcome, query: &Query) {
    let clock = ev_core::local_clock();
    println!("{{");
    println!("  \"total\": {},", outcome.total);
    println!("  \"count\": {},", outcome.ids.len());
    println!(
        "  \"elapsed_ms\": {:.3},",
        outcome.elapsed.as_secs_f64() * 1000.0
    );
    println!("  \"query\": \"{}\",", json_escape(&query.raw));
    println!("  \"results\": [");
    for (i, &id) in outcome.ids.iter().enumerate() {
        let e = index.entry(id);
        let stat = index.cached_stat(id);
        let (size, modified) = match stat {
            Some(s) => (s.size, format_filetime(s.mtime, &clock)),
            None => (0, String::new()),
        };
        println!(
            "    {{\"name\": \"{}\", \"path\": \"{}\", \"size\": {}, \"modified\": \"{}\", \"is_dir\": {}, \"frn\": {}}}{}",
            json_escape(index.name(id)),
            json_escape(index.path(id)),
            size,
            modified,
            e.is_dir(),
            e.frn,
            if i + 1 == outcome.ids.len() { "" } else { "," }
        );
    }
    println!("  ]");
    println!("}}");
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn watch(index: &mut Index, volumes: &[char]) {
    let mut watcher = JournalWatcher::start(volumes);
    if watcher.is_empty() {
        eprintln!("ev: no USN journal available (administrator privileges required)");
        return;
    }
    eprintln!("ev: watching for changes (ctrl+c to stop)");
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let batch = watcher.poll();
        if batch.is_empty() {
            continue;
        }
        let mut upserts = Vec::new();
        let mut removes = Vec::new();
        for (_letter, d) in batch {
            for u in d.upserts {
                println!("+ {}", u.name);
                upserts.push(u);
            }
            for r in d.removes {
                println!("- frn {r}");
                removes.push(r);
            }
        }
        index.apply_delta(&upserts, &removes);
        eprintln!("ev: {} objects", thousands(index.len()));
    }
}

#[allow(dead_code)]
fn default_sort() -> (SortKey, bool) {
    (SortKey::Name, false)
}

