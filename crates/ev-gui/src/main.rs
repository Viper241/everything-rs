//! everything-rs - a Rust reimplementation of Everything's instant file search.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use eframe::egui;
use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, Sense, Stroke, Vec2};

use ev_core::index::Id;
use ev_core::{Index, Query, SortKey, SortSpec, format_filetime, format_size, thousands};
use ev_ntfs::JournalWatcher;

const ACCENT: Color32 = Color32::from_rgb(0x4c, 0x8d, 0xff);
const ROW_HEIGHT: f32 = 23.0;
const ROW_FONT: f32 = 13.5;
const DISPLAY_LIMIT: usize = 50_000;

// ---------------------------------------------------------------- messages

struct SearchRequest {
    id: u64,
    query: Query,
    sort: SortSpec,
    limit: usize,
    cancel: Arc<AtomicBool>,
}

struct SearchResponse {
    id: u64,
    ids: Vec<Id>,
    total: usize,
    elapsed_ms: f64,
    incomplete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Loading,
    Scanning,
    Ready,
}

struct Status {
    phase: Phase,
    message: String,
    mode: String,
    elapsed_ms: f64,
    cache_hit: bool,
}

struct Shared {
    index: RwLock<Index>,
    generation: AtomicU64,
    counter: Arc<AtomicU64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Files,
    Folders,
}

#[derive(Clone, Copy)]
enum RowAction {
    Open,
    Reveal,
    Copy,
}

struct Config {
    volumes: Vec<char>,
    folders: Vec<String>,
    db: PathBuf,
    search: String,
}

impl Config {
    fn load() -> Self {
        let mut cfg = Config {
            volumes: Vec::new(),
            folders: Vec::new(),
            db: default_db_path(),
            search: String::new(),
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--volumes" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        cfg.volumes = v
                            .split([',', ';'])
                            .filter_map(|s| s.trim().chars().next())
                            .map(|c| c.to_ascii_uppercase())
                            .collect();
                    }
                }
                "--folders" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        cfg.folders = v
                            .split(';')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    }
                }
                "--db" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        cfg.db = PathBuf::from(v);
                    }
                }
                "-s" | "--search" => {
                    i += 1;
                    if let Some(v) = args.get(i) {
                        cfg.search = v.clone();
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if cfg.volumes.is_empty() && cfg.folders.is_empty() {
            cfg.volumes = ev_ntfs::list_volumes()
                .into_iter()
                .filter(|v| v.kind == "fixed")
                .map(|v| v.letter)
                .collect();
            if cfg.volumes.is_empty() {
                cfg.volumes = ev_ntfs::list_volumes().into_iter().map(|v| v.letter).collect();
            }
        }
        cfg
    }
}

fn default_db_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("everything-rs").join("index.evdb")
}

// --------------------------------------------------------------------- app

struct App {
    ctx: egui::Context,
    shared: Arc<Shared>,
    status: Arc<Mutex<Status>>,
    search_tx: Sender<SearchRequest>,
    search_rx: Receiver<SearchResponse>,
    pending_cancel: Option<Arc<AtomicBool>>,
    pending: bool,
    request_id: u64,
    applied_id: u64,

    query: String,
    filter: Filter,
    sort: SortSpec,
    results: Vec<Id>,
    total: usize,
    elapsed_ms: f64,
    incomplete: bool,
    warnings: Vec<String>,
    highlight: Vec<String>,

    selected: Option<usize>,
    scroll_to: Option<usize>,
    dirty: bool,
    last_generation: u64,
    focus_search: bool,
    show_help: bool,
    db: PathBuf,
    clipboard_request: Option<String>,
    volumes: Vec<char>,
    folders: Vec<String>,
    search_worker: Option<std::thread::JoinHandle<()>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        setup_style(&cc.egui_ctx);
        install_fonts(&cc.egui_ctx);

        let shared = Arc::new(Shared {
            index: RwLock::new(Index::builder().build()),
            generation: AtomicU64::new(0),
            counter: Arc::new(AtomicU64::new(0)),
        });
        let status = Arc::new(Mutex::new(Status {
            phase: Phase::Loading,
            message: "Starting…".into(),
            mode: String::new(),
            elapsed_ms: 0.0,
            cache_hit: false,
        }));

        spawn_indexer(
            shared.clone(),
            status.clone(),
            cfg.volumes.clone(),
            cfg.folders.clone(),
            cfg.db.clone(),
            cc.egui_ctx.clone(),
        );
        let (search_tx, search_rx, worker) = spawn_search_worker(shared.clone());

        let mut app = App {
            ctx: cc.egui_ctx.clone(),
            shared,
            status,
            search_tx,
            search_rx,
            pending_cancel: None,
            pending: true,
            request_id: 0,
            applied_id: 0,
            query: cfg.search,
            filter: Filter::All,
            sort: SortSpec::default(),
            results: Vec::new(),
            total: 0,
            elapsed_ms: 0.0,
            incomplete: false,
            warnings: Vec::new(),
            highlight: Vec::new(),
            selected: None,
            scroll_to: None,
            dirty: true,
            last_generation: u64::MAX,
            focus_search: true,
            show_help: false,
            db: cfg.db,
            clipboard_request: None,
            volumes: cfg.volumes.clone(),
            folders: cfg.folders.clone(),
            search_worker: Some(worker),
        };
        app.dispatch();
        app
    }

    fn effective_query(&self) -> String {
        let mut q = self.query.clone();
        match self.filter {
            Filter::All => {}
            Filter::Files => q.push_str(" file:"),
            Filter::Folders => q.push_str(" folder:"),
        }
        q
    }

    fn dispatch(&mut self) {
        if let Some(c) = self.pending_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.pending_cancel = Some(cancel.clone());
        self.request_id += 1;
        let generation = self.shared.generation.load(Ordering::Relaxed);
        let text = self.effective_query();
        let query = Query::parse(&text, &ev_core::local_clock());
        self.warnings = query.warnings.clone();
        self.highlight = plain_terms(&query);
        let _ = self.search_tx.send(SearchRequest {
            id: self.request_id,
            query,
            sort: self.sort,
            limit: DISPLAY_LIMIT,
            cancel,
        });
        self.last_generation = generation;
        self.pending = true;
        self.dirty = false;
    }

    fn poll_results(&mut self) {
        while let Ok(resp) = self.search_rx.try_recv() {
            if resp.id < self.applied_id {
                continue;
            }
            self.applied_id = resp.id;
            self.pending = false;
            self.results = resp.ids;
            self.total = resp.total;
            self.elapsed_ms = resp.elapsed_ms;
            self.incomplete = resp.incomplete;
            if self
                .selected
                .is_some_and(|s| s >= self.results.len())
            {
                self.selected = if self.results.is_empty() {
                    None
                } else {
                    Some(self.results.len() - 1)
                };
            }
        }
    }

    fn reindex(&mut self) {
        let _ = std::fs::remove_file(&self.db);
        {
            let mut s = self.status.lock().unwrap();
            s.phase = Phase::Scanning;
            s.message = "Reindexing…".into();
            s.cache_hit = false;
            s.mode.clear();
        }
        self.shared.counter.store(0, Ordering::Relaxed);
        spawn_indexer(
            self.shared.clone(),
            self.status.clone(),
            self.volumes.clone(),
            self.folders.clone(),
            self.db.clone(),
            self.ctx.clone(),
        );
    }

    fn open_selected(&self, reveal: bool) {
        let Some(sel) = self.selected else { return };
        let index = self.shared.index.read().unwrap();
        let Some(&id) = self.results.get(sel) else { return };
        let path = index.path(id).to_string();
        drop(index);
        if reveal {
            reveal_in_explorer(&path);
        } else {
            shell_open(&path);
        }
    }

    fn copy_selected(&mut self, with_path: bool) {
        let Some(sel) = self.selected else { return };
        let index = self.shared.index.read().unwrap();
        let Some(&id) = self.results.get(sel) else { return };
        let text = if with_path {
            index.path(id).to_string()
        } else {
            index.name(id).to_string()
        };
        drop(index);
        self.clipboard_request = Some(text);
    }

    fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard_request.take()
    }
}

fn plain_terms(query: &Query) -> Vec<String> {
    use ev_core::pattern::Kind;
    use ev_core::query::{Leaf, Node};
    fn walk(n: &Node, out: &mut Vec<String>) {
        match n {
            Node::And(items) | Node::Or(items) => items.iter().for_each(|i| walk(i, out)),
            Node::Not(b) => walk(b, out),
            Node::Leaf(Leaf::Name(m)) => {
                if m.kind == Kind::Contains && !m.text.is_empty() {
                    out.push(m.text.to_ascii_lowercase());
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&query.root, &mut out);
    out
}

// ----------------------------------------------------------------- workers

fn spawn_search_worker(
    shared: Arc<Shared>,
) -> (Sender<SearchRequest>, Receiver<SearchResponse>, std::thread::JoinHandle<()>) {
    let (req_tx, req_rx) = channel::<SearchRequest>();
    let (res_tx, res_rx) = channel::<SearchResponse>();
    let handle = std::thread::spawn(move || {
        while let Ok(req) = req_rx.recv() {
            if req.cancel.load(Ordering::Relaxed) {
                continue;
            }
            let index = shared.index.read().unwrap();
            let outcome = index.search(&req.query, req.sort, req.limit, &req.cancel);
            drop(index);
            if outcome.cancelled {
                continue;
            }
            let _ = res_tx.send(SearchResponse {
                id: req.id,
                ids: outcome.ids,
                total: outcome.total,
                elapsed_ms: outcome.elapsed.as_secs_f64() * 1000.0,
                incomplete: outcome.stats_incomplete,
            });
        }
    });
    (req_tx, res_rx, handle)
}

fn spawn_indexer(
    shared: Arc<Shared>,
    status: Arc<Mutex<Status>>,
    volumes: Vec<char>,
    folders: Vec<String>,
    db: PathBuf,
    ctx: egui::Context,
) {
    let counter = shared.counter.clone();

    std::thread::spawn(move || {
        let mut cached: Option<Index> = None;
        if folders.is_empty() && db.exists() {
            if let Ok(mut idx) = Index::load(&db) {
                if ev_ntfs::cache_usable(&idx, &volumes) {
                    let marks = idx.journal.clone();
                    if !marks.is_empty() {
                        // Journal available: replay everything that changed
                        // while the app was closed.
                        if marks.len() == volumes.len() && ev_ntfs::catch_up(&mut idx, &marks) {
                            cached = Some(idx);
                        }
                    } else {
                        // Unprivileged: no journal access. Serve a recent
                        // snapshot instead of a multi-minute rescan.
                        let age = now_unix().saturating_sub(idx.built_unix);
                        if age < 3600 {
                            cached = Some(idx);
                        }
                    }
                }
            }
        }

        let (mut index, from_cache) = match cached {
            Some(idx) => {
                counter.store(idx.len() as u64, Ordering::Relaxed);
                status.lock().unwrap().cache_hit = true;
                (idx, true)
            }
            None => {
                {
                    let mut s = status.lock().unwrap();
                    s.phase = Phase::Scanning;
                    s.message = if folders.is_empty() {
                        format!("Indexing {}…", display_volumes(&volumes))
                    } else {
                        "Indexing folders…".into()
                    };
                }
                let (idx, report) = if folders.is_empty() {
                    ev_ntfs::scan_volumes(&volumes, &counter)
                } else {
                    ev_ntfs::scan_folders(&folders, &counter)
                };
                {
                    let mut s = status.lock().unwrap();
                    s.elapsed_ms = report.elapsed.as_secs_f64() * 1000.0;
                    s.mode = report
                        .modes
                        .iter()
                        .map(|(l, m)| format!("{l}: {m}"))
                        .collect::<Vec<_>>()
                        .join("  ");
                    if let Some(err) = report.errors.first() {
                        s.message = err.clone();
                    }
                }
                (idx, false)
            }
        };

        if volumes.is_empty() {
            index.journal.clear();
        } else {
            index.journal = ev_ntfs::journal_marks(&volumes);
        }
        {
            let mut g = shared.index.write().unwrap();
            *g = index;
        }
        shared.generation.fetch_add(1, Ordering::Release);
        ctx.request_repaint();
        {
            let mut s = status.lock().unwrap();
            s.phase = Phase::Ready;
            s.elapsed_ms = if from_cache { 0.0 } else { s.elapsed_ms };
        }

        // Persist so the next start is instant.
        if folders.is_empty() {
            let g = shared.index.read().unwrap();
            let _ = g.save(&db);
        }

        if volumes.is_empty() {
            return;
        }

        let mut watcher = JournalWatcher::start(&volumes);
        if watcher.is_empty() {
            return;
        }
        let mut since_save = std::time::Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let batch = watcher.poll();
            if batch.is_empty() {
                continue;
            }
            let mut changed = false;
            {
                let mut g = shared.index.write().unwrap();
                for (_letter, d) in batch {
                    g.apply_delta(&d.upserts, &d.removes);
                    changed = true;
                }
            }
            if changed {
                shared.generation.fetch_add(1, Ordering::Release);
                ctx.request_repaint();
                counter.store(
                    shared.index.read().unwrap().len() as u64,
                    Ordering::Relaxed,
                );
            }
            if since_save.elapsed() > Duration::from_secs(120) {
                since_save = std::time::Instant::now();
                let mut g = shared.index.write().unwrap();
                g.journal = ev_ntfs::journal_marks(&volumes);
                let _ = g.save(&db);
            }
        }
    });
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn display_volumes(vols: &[char]) -> String {
    vols.iter()
        .map(|c| format!("{c}:"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------- painting

fn setup_style(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(0x18, 0x19, 0x1c);
    visuals.window_fill = Color32::from_rgb(0x20, 0x22, 0x26);
    visuals.extreme_bg_color = Color32::from_rgb(0x11, 0x12, 0x15);
    visuals.faint_bg_color = Color32::from_rgb(0x20, 0x22, 0x26);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(0xc8, 0xda, 0xff));
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0x2c, 0x2f, 0x35));
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x24, 0x27, 0x2c);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x24, 0x27, 0x2c);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2e, 0x32, 0x39);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x2e, 0x32, 0x39);
    visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.7);
    visuals.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.7);
    visuals.override_text_color = Some(Color32::from_rgb(0xd8, 0xdb, 0xe0));
    ctx.set_visuals(visuals.clone());
    ctx.all_styles_mut(|style| {
        style.visuals = visuals.clone();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
        style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
        style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);
        style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(6);
    });
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates: [(&str, &str, u32); 3] = [
        ("segoeui", r"C:\Windows\Fonts\segoeui.ttf", 0),
        ("segoeuib", r"C:\Windows\Fonts\segoeuib.ttf", 0),
        ("msyh", r"C:\Windows\Fonts\msyh.ttc", 0),
    ];
    let mut added = 0;
    for (name, path, index) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut data = egui::FontData::from_owned(bytes);
            data.index = index;
            fonts
                .font_data
                .insert(name.to_owned(), std::sync::Arc::new(data));
            let prop = fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default();
            if added == 0 {
                prop.insert(0, name.to_owned());
            } else {
                prop.push(name.to_owned());
            }
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push(name.to_owned());
            added += 1;
        }
    }
    if added > 0 {
        ctx.set_fonts(fonts);
    }
}

fn shell_open(path: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    let file: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let op: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

fn reveal_in_explorer(path: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn();
}

// -------------------------------------------------------------------- view

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        // Keyboard first, so arrows move the result selection even while the
        // search box has focus (Everything behaves the same way).
        let down = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown));
        let up = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp));
        let pgdn = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown));
        let pgup = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp));
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let reindex = ctx.input(|i| i.key_pressed(egui::Key::F5));
        let copy = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::C));
        let reveal_k = ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::R));
        if let Some(text) = self.take_clipboard() {
            ctx.copy_text(text);
        }
        if esc {
            if !self.query.is_empty() {
                self.query.clear();
                self.dirty = true;
            }
            self.focus_search = true;
        }
        if reindex {
            self.reindex();
        }
        if copy {
            self.copy_selected(true);
        }
        if reveal_k {
            self.open_selected(true);
        }

        if down || up || pgdn || pgup {
            let len = self.results.len();
            if len > 0 {
                let step = 20usize;
                let cur = self.selected.unwrap_or(0) as i64;
                let delta = if down {
                    1
                } else if up {
                    -1
                } else if pgdn {
                    step as i64
                } else {
                    -(step as i64)
                };
                let next = if self.selected.is_none() && delta < 0 {
                    0
                } else {
                    (cur + delta).clamp(0, len as i64 - 1)
                };
                self.selected = Some(next as usize);
                self.scroll_to = self.selected;
            }
        }
        if enter {
            self.open_selected(false);
        }

        let generation = self.shared.generation.load(Ordering::Relaxed);
        if generation != self.last_generation {
            self.dirty = true;
        }
        if self.dirty {
            self.dispatch();
        }
        self.poll_results();

        self.top_bar(ui);
        self.status_bar(ui);
        self.result_area(ui);

        if self.show_help {
            self.help_window(ctx);
        }

        // Keep animating while indexing and poll promptly while a search is in
        // flight: the worker cannot wake the UI by itself.
        let phase = self.status.lock().unwrap().phase;
        if phase != Phase::Ready {
            ctx.request_repaint_after(Duration::from_millis(100));
        } else if self.pending {
            ctx.request_repaint_after(Duration::from_millis(8));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Ok(g) = self.shared.index.write() {
            let _ = g.save(&self.db);
        }
        if let Some(h) = self.search_worker.take() {
            drop(h);
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(0x1e, 0x20, 0x24))
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let edit = egui::TextEdit::singleline(&mut self.query)
                        .hint_text("Search everything…")
                        .font(FontId::proportional(16.0))
                        .margin(egui::Margin::symmetric(12, 9))
                        .desired_width(f32::INFINITY);
                    let resp = ui.add(edit);
                    if self.focus_search {
                        resp.request_focus();
                        self.focus_search = false;
                    }
                    if resp.changed() {
                        self.dirty = true;
                    }
                    if !self.query.is_empty()
                        && ui
                            .add(egui::Button::new("✕").frame(false))
                            .on_hover_text("Clear (Esc)")
                            .clicked()
                    {
                        self.query.clear();
                        self.dirty = true;
                        self.focus_search = true;
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (value, label) in [
                        (Filter::All, "All"),
                        (Filter::Files, "Files"),
                        (Filter::Folders, "Folders"),
                    ] {
                        if ui
                            .selectable_label(self.filter == value, label)
                            .clicked()
                            && self.filter != value
                        {
                            self.filter = value;
                            self.dirty = true;
                        }
                    }
                    ui.add_space(10.0);
                    let ready = self.status.lock().unwrap().phase == Phase::Ready;
                    if ready {
                        ui.label(
                            egui::RichText::new(format!("{} objects", thousands(self.total)))
                                .strong()
                                .color(Color32::from_rgb(0xb6, 0xbc, 0xc6)),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new("indexing…")
                                .strong()
                                .color(Color32::from_rgb(0x86, 0x8d, 0x99)),
                        );
                    }
                    if self.elapsed_ms > 0.0 {
                        ui.label(
                            egui::RichText::new(format!("· {:.1} ms", self.elapsed_ms))
                                .color(Color32::from_rgb(0x86, 0x8d, 0x99)),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Help").clicked() {
                            self.show_help = !self.show_help;
                        }
                        if ui
                            .button(if self.sort.desc { "↓" } else { "↑" })
                            .on_hover_text("Toggle sort direction")
                            .clicked()
                        {
                            self.sort.desc = !self.sort.desc;
                            self.dirty = true;
                        }
                        egui::ComboBox::from_id_salt("sort")
                            .width(140.0)
                            .selected_text(sort_label(self.sort.key))
                            .show_ui(ui, |ui| {
                                for (key, label) in ev_core::search::ALL_SORTS {
                                    if ui
                                        .selectable_label(self.sort.key == *key, *label)
                                        .clicked()
                                    {
                                        self.sort.key = *key;
                                        self.dirty = true;
                                    }
                                }
                            });
                        ui.label(
                            egui::RichText::new("Sort")
                                .color(Color32::from_rgb(0x86, 0x8d, 0x99)),
                        );
                    });
                });

                if let Some(w) = self.warnings.first() {
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new(format!("⚠ {w}"))
                            .small()
                            .color(Color32::from_rgb(0xe0, 0xa0, 0x50)),
                    );
                }
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::NONE
                    .fill(Color32::from_rgb(0x15, 0x16, 0x19))
                    .inner_margin(egui::Margin::symmetric(14, 5)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let status = self.status.lock().unwrap();
                    match status.phase {
                        Phase::Ready => {
                            let g = self.shared.index.read().unwrap();
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} objects · {} MB",
                                    thousands(g.len()),
                                    g.memory_bytes() / (1024 * 1024)
                                ))
                                .small()
                                .color(Color32::from_rgb(0x9a, 0xa1, 0xac)),
                            );
                            if !status.mode.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("· {}", status.mode))
                                        .small()
                                        .color(Color32::from_rgb(0x6f, 0x76, 0x81)),
                                );
                            }
                            if status.cache_hit {
                                ui.label(
                                    egui::RichText::new("· loaded from cache")
                                        .small()
                                        .color(Color32::from_rgb(0x6f, 0x76, 0x81)),
                                );
                            }
                            if self.incomplete {
                                ui.label(
                                    egui::RichText::new("· metadata pending")
                                        .small()
                                        .color(Color32::from_rgb(0xc0, 0x90, 0x50)),
                                );
                            }
                        }
                        _ => {
                            ui.add(egui::Spinner::new().size(12.0));
                            ui.label(
                                egui::RichText::new(format!(
                                    "{}  {} objects",
                                    status.message,
                                    thousands(self.shared.counter.load(Ordering::Relaxed) as usize)
                                ))
                                .small()
                                .color(Color32::from_rgb(0xb6, 0xbc, 0xc6)),
                            );
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("Enter open · Ctrl+C path · Ctrl+R reveal · F5 reindex")
                                .small()
                                .color(Color32::from_rgb(0x5d, 0x64, 0x6e)),
                        );
                    });
                });
            });
    }

    fn result_area(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(0x18, 0x19, 0x1c)))
            .show(ui, |ui| {
                let width = ui.available_width();
                let (name_w, path_w, size_w, date_w) = columns(width);
                self.header_row(ui, name_w, path_w, size_w, date_w);
                ui.separator();

                if self.results.is_empty() {
                    self.empty_state(ui);
                    return;
                }

                let rows = self.results.len();
                let highlight = self.highlight.clone();
                let selected = self.selected;
                let scroll_target = self.scroll_to.take();
                let mut clicked: Option<usize> = None;
                let mut double: Option<usize> = None;
                let mut menu_action: Option<(RowAction, String)> = None;

                {
                    let index = self.shared.index.read().unwrap();
                    let row_height = ROW_HEIGHT;
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show_rows(ui, row_height, rows, |ui, range| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for i in range {
                                let id = self.results[i];
                                let (rect, resp) = ui.allocate_exact_size(
                                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                                    Sense::click(),
                                );
                                if !ui.is_rect_visible(rect) {
                                    continue;
                                }
                                draw_row(
                                    ui,
                                    rect,
                                    &index,
                                    id,
                                    i,
                                    selected == Some(i),
                                    &highlight,
                                    name_w,
                                    path_w,
                                    size_w,
                                    date_w,
                                );
                                if resp.clicked() || resp.secondary_clicked() {
                                    clicked = Some(i);
                                }
                                if resp.double_clicked() {
                                    double = Some(i);
                                }
                                resp.context_menu(|ui| {
                                    if ui.button("Open").clicked() {
                                        menu_action =
                                            Some((RowAction::Open, index.path(id).to_string()));
                                        ui.close();
                                    }
                                    if ui.button("Open containing folder").clicked() {
                                        menu_action =
                                            Some((RowAction::Reveal, index.path(id).to_string()));
                                        ui.close();
                                    }
                                    ui.separator();
                                    if ui.button("Copy path").clicked() {
                                        menu_action =
                                            Some((RowAction::Copy, index.path(id).to_string()));
                                        ui.close();
                                    }
                                    if ui.button("Copy name").clicked() {
                                        menu_action =
                                            Some((RowAction::Copy, index.name(id).to_string()));
                                        ui.close();
                                    }
                                });
                            }

                            if let Some(i) = scroll_target {
                                ui.scroll_to_rect(
                                    Rect::from_min_size(
                                        egui::pos2(
                                            ui.min_rect().left(),
                                            ui.min_rect().top() + i as f32 * ROW_HEIGHT,
                                        ),
                                        Vec2::new(1.0, ROW_HEIGHT),
                                    ),
                                    Some(egui::Align::Center),
                                );
                            }
                        });
                }

                if let Some(i) = clicked {
                    self.selected = Some(i);
                }
                if let Some(i) = double {
                    self.selected = Some(i);
                    self.open_selected(false);
                }
                match menu_action {
                    Some((RowAction::Open, path)) => shell_open(&path),
                    Some((RowAction::Reveal, path)) => reveal_in_explorer(&path),
                    Some((RowAction::Copy, text)) => self.clipboard_request = Some(text),
                    _ => {}
                }
            });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            let phase = self.status.lock().unwrap().phase;
            if phase == Phase::Ready {
                ui.label(
                    egui::RichText::new("No results")
                        .size(18.0)
                        .color(Color32::from_rgb(0x7c, 0x83, 0x8e)),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Try fewer words, or press Esc to clear")
                        .color(Color32::from_rgb(0x5d, 0x64, 0x6e)),
                );
            } else {
                ui.spinner();
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Building the index…")
                        .color(Color32::from_rgb(0x7c, 0x83, 0x8e)),
                );
            }
        });
    }

    fn header_row(&mut self, ui: &mut egui::Ui, name_w: f32, path_w: f32, size_w: f32, date_w: f32) {
        let height = 22.0;
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), height),
            Sense::hover(),
        );
        let color = Color32::from_rgb(0x7f, 0x86, 0x92);
        let font = FontId::proportional(12.0);
        let mut x = rect.left() + 10.0;
        let mut header = |ui: &mut egui::Ui, label: &str, key: SortKey, x: f32, w: f32| {
            let r = Rect::from_min_size(
                egui::pos2(x, rect.top()),
                Vec2::new(w, height),
            );
            let resp = ui.interact(r, ui.make_persistent_id(("hdr", label)), Sense::click());
            let active = self.sort.key == key;
            let text = if active {
                format!("{label} {}", if self.sort.desc { "↓" } else { "↑" })
            } else {
                label.to_string()
            };
            ui.painter().text(
                egui::pos2(r.left(), r.center().y),
                egui::Align2::LEFT_CENTER,
                text,
                font.clone(),
                if active { Color32::from_rgb(0xc8, 0xd0, 0xdc) } else { color },
            );
            if resp.clicked() {
                if self.sort.key == key {
                    self.sort.desc = !self.sort.desc;
                } else {
                    self.sort.key = key;
                    self.sort.desc = false;
                }
                self.dirty = true;
            }
        };
        header(ui, "Name", SortKey::Name, x, name_w);
        x += name_w;
        header(ui, "Path", SortKey::Path, x, path_w);
        x += path_w;
        header(ui, "Size", SortKey::Size, x, size_w);
        x += size_w;
        header(ui, "Date Modified", SortKey::DateModified, x, date_w);
    }

    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_help;
        egui::Window::new("Search syntax")
            .open(&mut open)
            .resizable(true)
            .default_width(560.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (head, rows) in HELP {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(*head).strong().color(ACCENT));
                        for (a, b) in *rows {
                            ui.horizontal(|ui| {
                                ui.add_sized(
                                    [220.0, 18.0],
                                    egui::Label::new(
                                        egui::RichText::new(*a)
                                            .monospace()
                                            .color(Color32::from_rgb(0xd0, 0xd6, 0xe0)),
                                    ),
                                );
                                ui.label(
                                    egui::RichText::new(*b)
                                        .small()
                                        .color(Color32::from_rgb(0x9a, 0xa1, 0xac)),
                                );
                            });
                        }
                    }
                });
            });
        self.show_help = open;
    }
}

fn sort_label(key: SortKey) -> &'static str {
    ev_core::search::ALL_SORTS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, l)| *l)
        .unwrap_or("Name")
}

fn columns(width: f32) -> (f32, f32, f32, f32) {
    let size_w = 92.0;
    let date_w = 132.0;
    let rest = (width - size_w - date_w - 40.0).max(240.0);
    let name_w = rest * 0.44;
    let path_w = rest - name_w;
    (name_w, path_w, size_w, date_w)
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    ui: &mut egui::Ui,
    rect: Rect,
    index: &Index,
    id: Id,
    row: usize,
    selected: bool,
    highlight: &[String],
    name_w: f32,
    path_w: f32,
    size_w: f32,
    date_w: f32,
) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, egui::CornerRadius::same(3), ACCENT.gamma_multiply(0.32));
        painter.rect_stroke(
            rect,
            egui::CornerRadius::same(3),
            Stroke::new(1.0, ACCENT),
            egui::StrokeKind::Inside,
        );
    } else if row % 2 == 1 {
        painter.rect_filled(
            rect,
            0.0,
            Color32::from_rgb(0x1c, 0x1e, 0x22),
        );
    }

    let name = index.name(id);
    let path = index.path(id);
    let parent = parent_of(path, name);
    let is_dir = index.is_dir(id);
    let font = FontId::proportional(ROW_FONT);
    let size_font = FontId::monospace(ROW_FONT - 0.5);
    let text_color = if is_dir {
        Color32::from_rgb(0xc9, 0xd8, 0xf2)
    } else {
        Color32::from_rgb(0xdc, 0xdf, 0xe4)
    };
    let dim = Color32::from_rgb(0x84, 0x8b, 0x97);

    let cy = rect.center().y;
    let mut x = rect.left() + 10.0;

    // Name (with match highlighting)
    let cell = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(x + name_w - 8.0, rect.bottom()),
    );
    let job = highlighted(name, highlight, font.clone(), text_color);
    let galley = painter.layout_job(job);
    painter
        .with_clip_rect(cell)
        .galley(egui::pos2(x, cy - galley.size().y * 0.5), galley, text_color);
    x += name_w;

    // Path
    let cell = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(x + path_w - 8.0, rect.bottom()),
    );
    let galley = painter.layout_no_wrap(parent.to_string(), font.clone(), dim);
    painter
        .with_clip_rect(cell)
        .galley(egui::pos2(x, cy - galley.size().y * 0.5), galley, dim);
    x += path_w;

    // Size
    let stat = index.cached_stat(id).or_else(|| index.ensure_stat(id));
    let size_text = if is_dir {
        String::new()
    } else {
        stat.map(|s| format_size(s.size)).unwrap_or_default()
    };
    let cell = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(x + size_w, rect.bottom()),
    );
    let galley = painter.layout_no_wrap(size_text, size_font, dim);
    let right = cell.right() - 8.0 - galley.size().x;
    painter
        .with_clip_rect(cell)
        .galley(egui::pos2(right, cy - galley.size().y * 0.5), galley, dim);
    x += size_w;

    // Date
    let date_text = stat
        .map(|s| format_filetime(s.mtime, &index.clock))
        .unwrap_or_default();
    let cell = Rect::from_min_max(
        egui::pos2(x, rect.top()),
        egui::pos2(x + date_w, rect.bottom()),
    );
    let galley = painter.layout_no_wrap(date_text, font, dim);
    painter
        .with_clip_rect(cell)
        .galley(egui::pos2(x, cy - galley.size().y * 0.5), galley, dim);
}

fn parent_of<'a>(path: &'a str, name: &str) -> &'a str {
    let cut = path.len().saturating_sub(name.len());
    let mut p = &path[..cut];
    while p.ends_with('\\') && p.len() > 3 {
        p = &p[..p.len() - 1];
    }
    p
}

fn highlighted(text: &str, needles: &[String], font: FontId, color: Color32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let lower = text.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for n in needles {
        if n.is_empty() {
            continue;
        }
        let mut from = 0usize;
        while let Some(pos) = lower[from..].find(n.as_str()) {
            let start = from + pos;
            ranges.push((start, start + n.len()));
            from = start + n.len().max(1);
            if ranges.len() >= 16 {
                break;
            }
        }
    }
    ranges.sort_unstable();
    let mut cursor = 0usize;
    for (start, end) in ranges {
        if start < cursor || end > text.len() {
            continue;
        }
        if start > cursor {
            job.append(
                &text[cursor..start],
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color,
                    ..Default::default()
                },
            );
        }
        job.append(
            &text[start..end],
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: Color32::from_rgb(0xff, 0xdd, 0x9a),
                background: Color32::from_rgb(0x4a, 0x3a, 0x12),
                underline: Stroke::new(1.0, Color32::from_rgb(0xe0, 0xa8, 0x40)),
                ..Default::default()
            },
        );
        cursor = end;
    }
    if cursor < text.len() {
        job.append(
            &text[cursor..],
            0.0,
            egui::TextFormat {
                font_id: font,
                color,
                ..Default::default()
            },
        );
    }
    job
}

const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Operators",
        &[
            ("space", "AND - both terms must match"),
            ("|", "OR - either term"),
            ("!", "NOT - exclude the term"),
            ("\"exact phrase\"", "keep spaces inside one term"),
            ("(a b) | c", "grouping"),
        ],
    ),
    (
        "Wildcards and regex",
        &[
            ("*.rs", "* matches any run of characters"),
            ("file?.txt", "? matches one character"),
            ("regex:^m.*\\.rs$", "regular expression"),
            ("case:Name", "match case for this term"),
            ("ww:foo", "whole words only"),
        ],
    ),
    (
        "Filters",
        &[
            ("ext:rs;toml", "extension list"),
            ("size:>1mb", "size (kb/mb/gb, ranges: 1mb..2mb)"),
            ("dm:today", "date modified"),
            ("dc:last7days", "date created"),
            ("da:2024-05", "date accessed"),
            ("attrib:HD", "attributes (H hidden, D directory, R readonly)"),
            ("len:<12", "file name length"),
            ("folder: / file:", "only folders / only files"),
            ("path:src", "match the full path"),
            ("parent:C:\\Windows", "direct children of a folder"),
            ("infolder:C:\\Windows", "the folder and everything below it"),
            ("depth:2 / root:", "nesting level / top level"),
            ("count:20", "limit results"),
        ],
    ),
    (
        "Duplicates and macros",
        &[
            ("dupe:", "files with duplicate names"),
            ("sizedupe:", "files with duplicate sizes"),
            ("dmdupe:", "same modified date"),
            ("audio: video: pic:", "media groups"),
            ("doc: exe: zip:", "document / executable / archive groups"),
            ("type:video", "same groups by name"),
        ],
    ),
];

fn main() -> eframe::Result {
    let cfg = Config::load();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1080.0, 640.0])
            .with_min_inner_size([640.0, 400.0])
            .with_position([24.0, 20.0])
            .with_title("everything-rs"),
        ..Default::default()
    };
    eframe::run_native(
        "everything-rs",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc, cfg)) as Box<dyn eframe::App>)),
    )
}
