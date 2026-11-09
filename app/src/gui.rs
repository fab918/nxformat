//! Native NXConvert window. Discovery and archive commands use the shared
//! packer APIs on workers; the UI owns presentation and artwork cropping.
use eframe::egui;
use nxformat_packer::{cli, CancellationToken};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;
mod artwork;
mod discovery;
mod theme;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use theme::{Icon, ACCENT, TEXT};

pub fn run() -> eframe::Result<()> {
    let icon = image::load_from_memory(include_bytes!("../assets/window-icon.png"))
        .expect("bundled icon")
        .to_rgba8();
    let icon = egui::IconData {
        width: icon.width(),
        height: icon.height(),
        rgba: icon.into_raw(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NXConvert")
            .with_icon(Arc::new(icon))
            // The configuration column keeps its width: the window never gets
            // narrow enough for the sources to run under it.
            .with_inner_size([1040.0, 680.0])
            .with_min_inner_size([900.0, 560.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "NXConvert",
        options,
        Box::new(|cc| {
            theme::setup(&cc.egui_ctx);
            Ok(Box::<App>::default())
        }),
    )
}

// ---- what the commands report ----

#[derive(Clone, Default)]
struct Content {
    platform: String,
    kind: String,
    application: u64,
    version: u32,
    display: String,
    name: String,
}

#[derive(Clone, Default)]
struct Source {
    tree: bool,
    icon: Option<Vec<u8>>,
    path: PathBuf,
    size: u64,
    firmware_bytes: u64,
    supported: bool,
    issue: String,
    contents: Vec<Content>,
    selected: bool,
}

#[derive(Clone, Default)]
struct Snapshot {
    identity: String,
    kind: String,
    application: u64,
    title: u64,
    version: u32,
    display: String,
    name: String,
}

#[derive(Clone, Default)]
struct Original {
    id: String,
    size: u64,
    name: String,
}

#[derive(Default)]
struct Archive {
    path: PathBuf,
    size: u64,
    reclaimable: u64,
    allocated: Option<u64>,
    snapshots: Vec<Snapshot>,
    originals: Vec<Original>,
}

// ---- running commands ----

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Scan,
    Pack,
    Contents,
    Edit,
}

struct Job {
    label: String,
    args: Vec<String>,
    purpose: Purpose,
    display: Option<nxformat_packer::DisplayFallback>,
}

enum Message {
    Sources(Vec<Source>),
    Cover(Vec<u8>),
    Line(bool, String),
    Done(Result<(), String>),
}

struct Running {
    job: Job,
    rx: mpsc::Receiver<Message>,
    cancel: CancellationToken,
    total: u64,
    done: u64,
    lines: Vec<String>,
    started: Instant,
}

fn start(job: Job, ctx: &egui::Context) -> Running {
    let (tx, rx) = mpsc::channel();
    let cancel = CancellationToken::default();
    let token = cancel.clone();
    let args = job.args.clone();
    let purpose = job.purpose;
    let display = job.display.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let lines = tx.clone();
        let repaint = ctx.clone();
        let sink: cli::Sink = Arc::new(move |error, line: &str| {
            let _ = lines.send(Message::Line(error, line.to_owned()));
            repaint.request_repaint();
        });
        let result = if purpose == Purpose::Scan {
            discovery::scan(args[1..].iter().map(PathBuf::from).collect(), &token).map(|sources| {
                let _ = tx.send(Message::Sources(sources));
            })
        } else {
            let result = cli::run_with_display(args.clone(), Some(sink), Some(token), display)
                .map_err(|e| e.to_string());
            if purpose == Purpose::Contents && result.is_ok() {
                let options = nxformat_packer::WriterOptions::default();
                if let Ok(summary) = nxformat_packer::inspect_archive(
                    &args[1],
                    &options,
                    nxformat_packer::IntegrityPolicy::Fast,
                ) {
                    if let Some(identity) = summary
                        .applications
                        .first()
                        .and_then(|a| a.metadata_snapshot_identity)
                    {
                        if let Ok(data) = nxformat_packer::read_snapshot_display(
                            &args[1], identity, &options, true,
                        ) {
                            if let Some(icon) = data.icon {
                                let _ = tx.send(Message::Cover(icon));
                            }
                        }
                    }
                }
            }
            result
        };
        let _ = tx.send(Message::Done(result));
        ctx.request_repaint();
    });
    Running {
        job,
        rx,
        cancel,
        total: 0,
        done: 0,
        lines: Vec::new(),
        started: Instant::now(),
    }
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| text(path))
}

/// A title from a file or folder name: "Pokemon Pokopia [0400A3A01EE4C000]"
/// gives "Pokemon Pokopia". Drops [..] (..) {..} tags, the extension and
/// anything from a version word ("v1.3.3") on.
fn title_from_path(path: &Path) -> String {
    let name = if path.is_dir() {
        path.file_name()
    } else {
        path.file_stem()
    };
    let name = name.map(|n| n.to_string_lossy()).unwrap_or_default();
    let mut plain = String::new();
    let mut depth = 0usize;
    for ch in name.chars() {
        match ch {
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            '_' if depth == 0 => plain.push(' '),
            _ if depth == 0 => plain.push(ch),
            _ => {}
        }
    }
    let is_version = |w: &str| {
        let mut c = w.chars();
        matches!(c.next(), Some('v' | 'V')) && c.next().is_some_and(|c| c.is_ascii_digit())
    };
    plain
        .split_whitespace()
        .take_while(|w| !is_version(w))
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c == '-' || c == '+' || c.is_whitespace())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{title_from_path, App, Content, Source};
    use std::path::Path;

    #[test]
    fn titles_from_names() {
        for (name, title) in [
            ("Pokemon Pokopia [0400A3A01EE4C000].nsp", "Pokemon Pokopia"),
            ("KirbyAirRiders_v1.3.3.xci", "KirbyAirRiders"),
            (
                "Mario Kart 8 Deluxe [v3.0.3] (EU).nsp",
                "Mario Kart 8 Deluxe",
            ),
            ("Super Mario Bros. Wonder.nsp", "Super Mario Bros. Wonder"),
            ("[0100000000010000].nsp", ""),
        ] {
            assert_eq!(title_from_path(Path::new(name)), title);
        }
    }
    #[test]
    fn firmware_controls_only_affect_games_with_a_detected_partition() {
        let folder = std::env::temp_dir().join(format!("nxconvert-options-{}", std::process::id()));
        let mut app = App {
            output_dir: Some(folder.clone()),
            sources: [("Card", 1, 32_000), ("Digital", 2, 0)]
                .into_iter()
                .map(|(name, application, firmware_bytes)| Source {
                    path: folder.join(name),
                    firmware_bytes,
                    selected: true,
                    supported: true,
                    contents: vec![Content {
                        application,
                        kind: "base".into(),
                        name: name.into(),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let card_key = app.games()[0].key.clone();
        for (global, game_override, expected) in [
            (true, false, false),
            (true, true, true),
            (false, false, true),
        ] {
            app.include_firmware = global;
            app.omitted_firmware.clear();
            if game_override {
                app.omitted_firmware.insert(card_key.clone());
            }
            app.convert();
            assert_eq!(app.queue.len(), 2);
            for flag in ["--omit-firmware", "--no-recipe"] {
                assert_eq!(app.queue[0].args.iter().any(|arg| arg == flag), expected);
                assert!(!app.queue[1].args.iter().any(|arg| arg == flag));
            }
            app.queue.clear();
        }
        // Only a selected native package counts; a deselected cart has no firmware.
        let games = app.games();
        assert!(app.native_package(&games[0]) && app.firmware_bytes(&games[0]) != 0);
        app.sources[0].selected = false;
        let games = app.games();
        assert!(!app.native_package(&games[0]) && app.firmware_bytes(&games[0]) == 0);
    }
}

fn bytes(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// A file name every system accepts, as NXEmu names its NX files.
fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_start().trim_end_matches([' ', '.']);
    if cleaned.is_empty() {
        "game".into()
    } else {
        cleaned.into()
    }
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "base" => "Game",
        "update" => "Update",
        "standalone" | "standalone-update" => "Complete update",
        "dlc" => "DLC",
        _ => "Other",
    }
}

fn is_nx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("nx"))
}

// ---- the window ----

struct Game {
    key: String,
    tree: bool,
    missing_title: bool,
    application: u64,
    name: String,
    sources: Vec<usize>,
    output: Option<PathBuf>,
    problem: String,
}

#[derive(Default)]
struct Appearance {
    title: String,
    icon: Option<Arc<[u8]>>,
    texture: Option<egui::TextureHandle>,
}

struct App {
    appearances: HashMap<String, Appearance>,
    textures: HashMap<PathBuf, egui::TextureHandle>,
    archive_cover: Option<egui::TextureHandle>,
    focused: String,
    crop: Option<artwork::Crop>,
    art_rx: Option<mpsc::Receiver<(String, Result<artwork::Artwork, String>)>>,
    platform: usize,
    last_output: Option<PathBuf>,
    remove_confirmation: Option<(String, Vec<String>)>,
    sources: Vec<Source>,
    output_dir: Option<PathBuf>,
    level: i32,
    no_compress: bool,
    recipe: bool,
    include_firmware: bool,
    omitted_firmware: HashSet<String>,
    archive: Option<Archive>,
    queue: VecDeque<Job>,
    running: Option<Running>,
    log: Vec<String>,
    status: String,
}

impl Default for App {
    fn default() -> Self {
        Self {
            appearances: HashMap::new(),
            textures: HashMap::new(),
            archive_cover: None,
            focused: String::new(),
            crop: None,
            art_rx: None,
            platform: 0,
            last_output: None,
            remove_confirmation: None,
            sources: Vec::new(),
            output_dir: None,
            level: 3,
            no_compress: false,
            recipe: true,
            include_firmware: true,
            omitted_firmware: HashSet::new(),
            archive: None,
            queue: VecDeque::new(),
            running: None,
            log: Vec::new(),
            status: String::new(),
        }
    }
}

impl App {
    fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    fn push(&mut self, label: impl Into<String>, args: Vec<String>, purpose: Purpose) {
        self.queue.push_back(Job {
            label: label.into(),
            args,
            purpose,
            display: None,
        });
    }

    fn scan(&mut self, paths: Vec<PathBuf>) {
        let fresh: Vec<String> = paths
            .iter()
            .filter(|p| !self.sources.iter().any(|s| &s.path == *p))
            .map(|p| text(p))
            .collect();
        if fresh.is_empty() {
            return;
        }
        self.archive = None;
        let mut args = vec!["scan".to_string()];
        args.extend(fresh);
        self.push("Reading the files", args, Purpose::Scan);
    }

    fn open_archive(&mut self, path: PathBuf) {
        self.archive_cover = None;
        self.archive = Some(Archive {
            path: path.clone(),
            ..Default::default()
        });
        self.push(
            format!("Reading {}", file_name(&path)),
            vec!["contents".into(), text(&path)],
            Purpose::Contents,
        );
    }

    fn reload_archive(&mut self) {
        if let Some(path) = self.archive.as_ref().map(|a| a.path.clone()) {
            self.push(
                "Reading the archive",
                vec!["contents".into(), text(&path)],
                Purpose::Contents,
            );
        }
    }

    fn drop_files(&mut self, files: Vec<PathBuf>) {
        if self.busy() {
            self.status = "Wait for the current operation before adding more files.".into();
            return;
        }
        let (images, files): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| {
            p.extension().is_some_and(|e| {
                ["png", "jpg", "jpeg"]
                    .iter()
                    .any(|v| e.eq_ignore_ascii_case(v))
            })
        });
        if let Some(path) = images.first() {
            let key = self.focused.clone();
            if !key.is_empty() {
                self.load_artwork(key, path.clone());
            } else {
                self.status = "Add a game before choosing its artwork.".into();
            }
        }
        let (nx, others): (Vec<PathBuf>, Vec<PathBuf>) = files.into_iter().partition(|p| is_nx(p));
        if !others.is_empty() {
            self.scan(others);
        }
        if let Some(path) = nx.into_iter().next() {
            self.open_archive(path);
        }
    }

    /// The sources grouped by application, as NXEmu groups them.
    fn games(&self) -> Vec<Game> {
        let mut games: Vec<Game> = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            let Some(first) = source.contents.first() else {
                continue;
            };
            let at = match games.iter().position(|g| {
                g.key
                    == if source.tree {
                        text(&source.path)
                    } else {
                        format!("{}:{:016X}", first.platform, first.application)
                    }
            }) {
                Some(at) => at,
                None => {
                    games.push(Game {
                        key: if source.tree {
                            text(&source.path)
                        } else {
                            format!("{}:{:016X}", first.platform, first.application)
                        },
                        tree: source.tree,
                        missing_title: true,
                        application: first.application,
                        name: String::new(),
                        sources: Vec::new(),
                        output: None,
                        problem: String::new(),
                    });
                    games.len() - 1
                }
            };
            let game = &mut games[at];
            game.sources.push(index);
            for c in &source.contents {
                if !c.name.is_empty()
                    && (game.name.is_empty() || c.kind == "base" || c.kind == "standalone")
                {
                    game.name = c.name.clone();
                }
            }
        }
        for game in &mut games {
            game.missing_title = game.name.is_empty();
            if let Some(appearance) = self.appearances.get(&game.key) {
                if game.missing_title && !appearance.title.trim().is_empty() {
                    game.name = appearance.title.trim().into();
                }
            }
            if game.name.is_empty() {
                game.name = game
                    .sources
                    .first()
                    .map(|&i| title_from_path(&self.sources[i].path))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "Untitled game".into());
            }
            let bases: Vec<&Source> = game
                .sources
                .iter()
                .map(|&i| &self.sources[i])
                .filter(|s| {
                    s.selected
                        && s.supported
                        && s.contents
                            .iter()
                            .any(|c| c.kind == "base" || c.kind == "standalone")
                })
                .collect();
            match bases.len() {
                // A source that already says why it has no base needs no second message.
                0 if game
                    .sources
                    .iter()
                    .all(|&i| !self.sources[i].issue.is_empty()) =>
                {
                    continue
                }
                0 => {
                    game.problem =
                        "Add the game itself (its base file): an NX holds the game with its updates and DLC."
                            .into();
                    continue;
                }
                1 => {}
                _ => {
                    game.problem = "Two files hold the game itself: keep only one of them.".into();
                    continue;
                }
            }
            let folder = self
                .output_dir
                .clone()
                .or_else(|| bases[0].path.parent().map(Path::to_path_buf))
                .unwrap_or_default();
            let output = folder.join(format!(
                "{} [{:016X}].nx",
                safe_name(&game.name),
                game.application
            ));
            if output.exists() {
                game.problem = format!(
                    "{} already exists. Open it in Contents to add updates or DLC.",
                    file_name(&output)
                );
            } else {
                game.output = Some(output);
            }
        }
        games
    }

    /// Selected, supported native packages (decrypted NSP or XCI): the only
    /// sources with rebuild data and bundled firmware. Extracted trees have none.
    fn native_package(&self, game: &Game) -> bool {
        !game.tree
            && game.sources.iter().any(|&index| {
                let source = &self.sources[index];
                source.selected && source.supported && !source.tree
            })
    }

    fn firmware_bytes(&self, game: &Game) -> u64 {
        game.sources
            .iter()
            .map(|&index| &self.sources[index])
            .filter(|source| source.selected && source.supported && !source.tree)
            .fold(0u64, |bytes, source| {
                bytes.saturating_add(source.firmware_bytes)
            })
    }

    fn omits_firmware(&self, game: &Game) -> bool {
        self.firmware_bytes(game) != 0
            && (!self.include_firmware || self.omitted_firmware.contains(&game.key))
    }

    fn remove_game(&mut self, game: &Game) {
        for &index in game.sources.iter().rev() {
            let source = self.sources.remove(index);
            self.textures.remove(&source.path);
        }
        self.appearances.remove(&game.key);
        self.omitted_firmware.remove(&game.key);
        if self.focused == game.key {
            self.focused.clear();
        }
    }

    fn convert(&mut self) {
        for game in self.games() {
            let Some(output) = &game.output else {
                continue;
            };
            let mut args = vec![if game.tree { "pack-tree" } else { "pack" }.to_string()];
            args.extend(
                game.sources
                    .iter()
                    .map(|&i| &self.sources[i])
                    .filter(|s| s.selected)
                    .map(|s| text(&s.path)),
            );
            args.push(text(output));
            if self.no_compress {
                args.push("--no-compress".into());
            } else {
                args.push("--level".into());
                args.push(self.level.to_string());
            }
            if !game.tree {
                let omit_firmware = self.omits_firmware(&game);
                if !self.recipe || omit_firmware {
                    args.push("--no-recipe".into());
                }
                if omit_firmware {
                    args.push("--omit-firmware".into());
                }
            }
            if game.tree && self.platform != 0 {
                args.extend([
                    "--platform".into(),
                    if self.platform == 1 {
                        "switch1"
                    } else {
                        "switch2"
                    }
                    .into(),
                ]);
            }
            if game.tree {
                // One NX file only: no review files beside it.
                args.push("--no-trail".into());
            }
            args.push("--frontend".into());
            self.push(format!("Converting {}", game.name), args, Purpose::Pack);
            // Missing fields only: the typed title, else the one from the file name.
            let appearance = self.appearances.get(&game.key);
            let title = appearance
                .map(|a| a.title.trim())
                .filter(|t| !t.is_empty())
                .or((game.missing_title && game.name != "Untitled game").then_some(&*game.name))
                .map(String::from);
            let icon = appearance
                .and_then(|a| a.icon.clone())
                .map(|b| (nxformat_packer::MediaType::JpegIcon, b));
            if title.is_some() || icon.is_some() {
                self.queue.back_mut().unwrap().display =
                    Some(nxformat_packer::DisplayFallback { title, icon });
            }
        }
    }

    fn edit(&mut self, label: String, mut args: Vec<String>) {
        args.push("--frontend".into());
        self.push(label, args, Purpose::Edit);
    }

    // ---- command results ----

    fn poll(&mut self, ctx: &egui::Context) {
        if self.running.is_none() {
            if let Some(job) = self.queue.pop_front() {
                self.status = job.label.clone();
                self.running = Some(start(job, ctx));
            }
        }
        if let Some(rx) = &self.art_rx {
            if let Ok((key, result)) = rx.try_recv() {
                self.art_rx = None;
                match result {
                    Ok(art) => {
                        self.crop = Some(artwork::Crop {
                            key,
                            art,
                            center: [0.5, 0.5],
                            zoom: 1.0,
                        })
                    }
                    Err(error) => self.status = error,
                }
            }
        }
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let mut finished = None;
        while let Ok(message) = running.rx.try_recv() {
            match message {
                Message::Sources(sources) => {
                    for mut source in sources {
                        if self.sources.iter().any(|s| s.path == source.path) {
                            continue;
                        }
                        if let Some(bytes) = source.icon.take() {
                            if let Ok(mut art) = artwork::Artwork::decode(&bytes) {
                                self.textures
                                    .insert(source.path.clone(), art.texture(ctx).clone());
                            }
                            source.icon = Some(bytes);
                        }
                        self.sources.push(source);
                    }
                }
                Message::Cover(bytes) => {
                    if let Ok(mut art) = artwork::Artwork::decode(&bytes) {
                        self.archive_cover = Some(art.texture(ctx).clone());
                    }
                }
                Message::Line(error, line) => {
                    if let Some(value) = line.strip_prefix("@TOTAL\t") {
                        // Each @TOTAL adds to what the command expects;
                        // @PROGRESS counts from the start of the command.
                        running.total += value.trim().parse::<u64>().unwrap_or(0);
                    } else if let Some(value) = line.strip_prefix("@PROGRESS\t") {
                        running.done = value.trim().parse().unwrap_or(running.done);
                    } else if line.starts_with('@') {
                        running.lines.push(line);
                    } else if !line.is_empty() {
                        self.log
                            .push(if error { format!("! {line}") } else { line });
                    }
                }
                Message::Done(result) => finished = Some(result),
            }
        }
        if let Some(result) = finished {
            let running = self.running.take().unwrap();
            // A successful command has already committed. A late cancel click
            // must not report that nothing changed or suppress its result.
            let cancelled = result.is_err() && running.cancel.is_cancelled();
            self.finish(running, result, cancelled);
            if !self.queue.is_empty() {
                ctx.request_repaint();
            }
        }
    }

    fn finish(&mut self, running: Running, result: Result<(), String>, cancelled: bool) {
        let records: Vec<Vec<&str>> = running
            .lines
            .iter()
            .map(|l| l[1..].split('\t').collect())
            .collect();
        match running.job.purpose {
            Purpose::Scan => self.scanned(&records, result.as_ref().err()),
            Purpose::Contents => self.contents(&records),
            Purpose::Pack | Purpose::Edit => {}
        }
        for r in &records {
            match r[..] {
                ["NOTE", path, note, ..] => self
                    .log
                    .push(format!("{}: {note}", file_name(Path::new(path)))),
                ["ISSUE", path, issue, ..] if running.job.purpose != Purpose::Scan => self
                    .log
                    .push(format!("! {}: {issue}", file_name(Path::new(path)))),
                ["SKIPPED", path, _, _, why, ..] => self
                    .log
                    .push(format!("{}: {why}", file_name(Path::new(path)))),
                ["REBUILT", path, size, ..] => self.log.push(format!(
                    "Rebuilt {} ({})",
                    file_name(Path::new(path)),
                    bytes(size.parse().unwrap_or(0))
                )),
                _ => {}
            }
        }
        self.status = match (&result, cancelled) {
            (_, true) => {
                self.queue.retain(|j| j.purpose == Purpose::Contents);
                format!("{}: cancelled, nothing was changed", running.job.label)
            }
            (Err(e), _) => {
                self.log.push(format!("! {}: {e}", running.job.label));
                format!("{}: failed", running.job.label)
            }
            (Ok(()), _) => {
                let output = running.job.args.iter().rev().find(|a| a.ends_with(".nx"));
                let size = output
                    .and_then(|o| std::fs::metadata(o).ok())
                    .map(|m| m.len());
                match (running.job.purpose, size) {
                    (Purpose::Pack, Some(size)) => {
                        let line = format!(
                            "Saved {} ({})",
                            output.map(|o| file_name(Path::new(o))).unwrap_or_default(),
                            bytes(size)
                        );
                        self.log.push(line.clone());
                        line
                    }
                    _ => format!("{}: done", running.job.label),
                }
            }
        };
        if running.job.purpose == Purpose::Edit {
            self.reload_archive();
        }
        if running.job.purpose == Purpose::Pack && result.is_ok() && !cancelled {
            self.last_output = running
                .job
                .args
                .iter()
                .find(|a| a.ends_with(".nx"))
                .map(PathBuf::from);
            // A converted game cannot be converted again; drop its sources.
            if let Some(output) = running.job.args.iter().rev().find(|a| a.ends_with(".nx")) {
                let used: Vec<&String> = running.job.args[1..]
                    .iter()
                    .take_while(|a| *a != output)
                    .collect();
                self.sources.retain(|s| !used.contains(&&text(&s.path)));
            }
        }
    }

    fn scanned(&mut self, records: &[Vec<&str>], error: Option<&String>) {
        let before = self.sources.len();
        for r in records {
            match r[..] {
                ["SOURCE", path, size, _, status, ..] => {
                    let path = PathBuf::from(path);
                    if self.sources.iter().any(|s| s.path == path) {
                        continue;
                    }
                    let supported = status == "Supported";
                    self.sources.push(Source {
                        path,
                        size: size.parse().unwrap_or(0),
                        supported,
                        selected: supported,
                        ..Default::default()
                    });
                }
                ["CONTENT", path, kind, _, application, version, display, name, ..] => {
                    let path = Path::new(path);
                    if let Some(s) = self.sources[before..].iter_mut().find(|s| s.path == path) {
                        s.contents.push(Content {
                            kind: kind.into(),
                            application: u64::from_str_radix(application, 16).unwrap_or(0),
                            version: version.parse().unwrap_or(0),
                            display: display.into(),
                            name: name.into(),
                            ..Default::default()
                        });
                    }
                }
                ["ISSUE", path, issue, ..] => {
                    let path = Path::new(path);
                    if let Some(s) = self.sources[before..].iter_mut().find(|s| s.path == path) {
                        if s.issue.is_empty() {
                            s.issue = issue.into();
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(error) = error {
            self.log.push(format!("! {error}"));
        }
    }

    fn contents(&mut self, records: &[Vec<&str>]) {
        let Some(archive) = self.archive.as_mut() else {
            return;
        };
        archive.snapshots.clear();
        archive.originals.clear();
        for r in records {
            match r[..] {
                ["SNAPSHOT", identity, kind, application, title, version, display, name, ..] => {
                    archive.snapshots.push(Snapshot {
                        identity: identity.into(),
                        kind: kind.into(),
                        application: u64::from_str_radix(application, 16).unwrap_or(0),
                        title: u64::from_str_radix(title, 16).unwrap_or(0),
                        version: version.parse().unwrap_or(0),
                        display: display.into(),
                        name: name.into(),
                    })
                }
                ["RECIPE", id, _, size, _, name, ..] => archive.originals.push(Original {
                    id: id.into(),
                    size: size.parse().unwrap_or(0),
                    name: name.into(),
                }),
                ["SIZE", size, ..] => archive.size = size.parse().unwrap_or(0),
                ["RECLAIMABLE", size, ..] => archive.reclaimable = size.parse().unwrap_or(0),
                ["ALLOCATED", size, ..] => archive.allocated = size.parse().ok(),
                _ => {}
            }
        }
    }

    fn load_artwork(&mut self, key: String, path: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.art_rx = Some(rx);
        std::thread::spawn(move || {
            let result = artwork::Artwork::load(&path);
            let _ = tx.send((key, result));
        });
    }
}

mod view;
