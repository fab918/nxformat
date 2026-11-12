//! Native NXConvert window. Discovery and archive commands use the shared
//! packer APIs on workers; the UI owns presentation and artwork cropping.
use eframe::egui;
use nxformat_packer::{
    cli, CancellationToken, IntegrityPolicy, MetadataEdit, Platform, WriterOptions,
};
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
    publisher: String,
}

#[derive(Clone, Default)]
struct Source {
    kind: SourceKind,
    tree_roots: Vec<String>,
    nup_platform: Option<Platform>,
    icon: Option<Vec<u8>>,
    path: PathBuf,
    size: u64,
    firmware_bytes: u64,
    supported: bool,
    issue: String,
    contents: Vec<Content>,
    selected: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum SourceKind {
    #[default]
    Native,
    Extracted,
    Nup,
}

#[derive(Clone, Default)]
struct Snapshot {
    identity: String,
    native: bool,
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
    metadata: Option<ArchiveMetadata>,
}

#[derive(Default)]
struct ArchiveMetadata {
    id: u64,
    editable: bool,
    title: String,
    publisher: String,
    icon: Option<Vec<u8>>,
    original: Option<Vec<u8>>,
    original_texture: Option<egui::TextureHandle>,
}

const ARCHIVE_ARTWORK: &str = "\0archive-artwork";

// ---- running commands ----

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Scan,
    Pack,
    Contents,
    Edit,
    Export,
}

struct Job {
    label: String,
    args: Vec<String>,
    purpose: Purpose,
    display: Option<nxformat_packer::DisplayFallback>,
    metadata: Option<MetadataEdit>,
    source_bytes: Option<u64>,
}

enum Message {
    Sources(Vec<Source>),
    Metadata(Option<ArchiveMetadata>),
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

fn start(mut job: Job, ctx: &egui::Context) -> Running {
    let (tx, rx) = mpsc::channel();
    let cancel = CancellationToken::default();
    let token = cancel.clone();
    let args = job.args.clone();
    let purpose = job.purpose;
    let display = job.display.clone();
    let metadata = job.metadata.take();
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
        } else if let Some(edit) = metadata {
            nxformat_packer::edit_archive_metadata(
                &args[1],
                vec![edit],
                &WriterOptions {
                    cancellation: token,
                    ..Default::default()
                },
                IntegrityPolicy::Fast,
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
        } else {
            let result =
                cli::run_with_display(args.clone(), Some(sink), Some(token.clone()), display)
                    .map_err(|e| e.to_string());
            if purpose == Purpose::Contents && result.is_ok() {
                let options = WriterOptions {
                    cancellation: token,
                    ..Default::default()
                };
                let metadata = read_archive_metadata(Path::new(&args[1]), &options);
                let _ = tx.send(Message::Metadata(metadata));
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

fn read_archive_metadata(path: &Path, options: &WriterOptions) -> Option<ArchiveMetadata> {
    let summary = nxformat_packer::inspect_archive(path, options, IntegrityPolicy::Fast).ok()?;
    let app = summary.applications.first()?;
    let editable = summary.applications.len() == 1;
    let display = match app.metadata_snapshot_identity {
        Some(identity) => {
            nxformat_packer::read_snapshot_display(path, identity, options, true).ok()?
        }
        None => Default::default(),
    };
    let original = if editable {
        nxformat_packer::read_original_icon(path, app.platform, app.application_id, options)
            .ok()
            .flatten()
    } else {
        None
    };
    Some(ArchiveMetadata {
        id: app.application_id,
        editable,
        title: display.title.unwrap_or_default(),
        publisher: display.publisher.unwrap_or_default(),
        icon: display.icon,
        original,
        ..Default::default()
    })
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

    #[test]
    fn nup_sources_convert_with_updates_dlc_and_metadata() {
        use super::{
            cli, discovery, Appearance, CancellationToken, IntegrityPolicy, Platform, WriterOptions,
        };
        use nxformat_packer::import::{
            structures::NpdmProfile,
            synthetic::npdm,
            synthetic_nup::{addon, base, build, update, Node},
        };
        use nxformat_packer::{inspect_archive, read_snapshot_display, SnapshotType};

        let dir = std::env::temp_dir().join(format!("nxconvert-nup-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        for (app_id, platform, standalone) in [
            (0x0100_0000_0001_0000, Platform::Switch1, false),
            (0x0400_0000_0001_0000, Platform::Switch2, false),
            (0x0400_0000_0002_0000, Platform::Switch2, true),
        ] {
            let source = dir.join(format!("{app_id:016x}.NUP"));
            let mut packages = vec![update(app_id, 65536, u8::from(standalone)), addon(app_id)];
            if !standalone {
                packages.insert(0, base(app_id));
            }
            if platform == Platform::Switch1 {
                for node in packages
                    .iter_mut()
                    .filter_map(|p| p.exefs.as_mut())
                    .flatten()
                {
                    if let Node::File("main.npdm", bytes) = node {
                        *bytes = npdm(app_id, NpdmProfile::MetaAcid2048V1);
                    }
                }
            }
            std::fs::write(&source, build(&packages, true)).unwrap();
            let token = CancellationToken::default();
            let selected_path = if platform == Platform::Switch1 {
                dir.clone()
            } else {
                source.clone()
            };
            let mut app = App {
                sources: discovery::scan(vec![selected_path], &token).unwrap(),
                ..Default::default()
            };
            assert_eq!(app.sources[0].nup_platform, Some(platform));
            assert_eq!(app.sources[0].contents.len(), packages.len());
            let game = app.games().remove(0);
            assert_eq!(game.default_title, "Synthetic Game");
            app.appearances.insert(
                game.key,
                Appearance {
                    title: "Custom NUP title".into(),
                    publisher: "Custom NUP publisher".into(),
                    ..Default::default()
                },
            );
            let output = app.games().remove(0).output.unwrap();
            app.convert();
            let job = app.queue.pop_front().unwrap();
            assert_eq!(job.args[0], "pack-nup");
            assert!(job.args.windows(2).any(|args| args == ["--level", "1"]));
            assert_eq!(
                job.source_bytes,
                Some(std::fs::metadata(&source).unwrap().len())
            );
            assert!(!job.args.iter().any(|arg| arg == "--skip-dlc"));
            cli::run_with_display(job.args, None, Some(token), job.display).unwrap();
            let options = WriterOptions::default();
            let summary = inspect_archive(&output, &options, IntegrityPolicy::Strict).unwrap();
            let application = &summary.applications[0];
            assert_eq!(
                (application.application_id, application.platform),
                (app_id, platform)
            );
            assert_eq!(application.snapshots.len(), packages.len());
            assert!(application
                .snapshots
                .iter()
                .any(|s| s.snapshot_type == SnapshotType::Dlc));
            let metadata = read_snapshot_display(
                &output,
                application.metadata_snapshot_identity.unwrap(),
                &options,
                false,
            )
            .unwrap();
            assert_eq!(metadata.title.as_deref(), Some("Custom NUP title"));
            assert_eq!(metadata.publisher.as_deref(), Some("Custom NUP publisher"));
            let snapshot = application
                .snapshots
                .iter()
                .find(|snapshot| snapshot.numeric_version == 65536)
                .unwrap();
            let extracted = dir.join(format!("{app_id:016x}-exefs"));
            let before = std::fs::read(&output).unwrap();
            nxformat_packer::extract_exefs(&output, snapshot.identity, &extracted, &options)
                .unwrap();
            assert!(std::fs::read(extracted.join("main"))
                .unwrap()
                .starts_with(b"NSO0"));
            assert_eq!(std::fs::read(&output).unwrap(), before);
            assert!(nxformat_packer::extract_exefs(
                &output,
                snapshot.identity,
                &extracted,
                &options
            )
            .is_err());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn nup_discovery_uses_the_conversion_inventory_limits() {
        use super::{discovery, CancellationToken};
        use nxformat_packer::import::synthetic_nup::{base, build, Node};

        let names: Vec<&str> = (0..1000)
            .map(|i| -> &'static str {
                Box::leak(format!("{i:04}-{}", "x".repeat(240)).into_boxed_str())
            })
            .collect();
        let mut package = base(0x0400_0000_0001_0000);
        let romfs = package.romfs.as_mut().unwrap();
        for i in 0..105 {
            let dir = Box::leak(format!("dir-{i:03}").into_boxed_str());
            romfs.push(Node::Dir(
                dir,
                names
                    .iter()
                    .map(|name| Node::File(name, Vec::new()))
                    .collect(),
            ));
        }
        let fixture = build(&[package], false);
        let toc_bytes = u64::from_le_bytes(fixture[16..24].try_into().unwrap());
        assert!(toc_bytes > 32 * 1024 * 1024);
        let path =
            std::env::temp_dir().join(format!("nxconvert-large-toc-{}.nup", std::process::id()));
        std::fs::write(&path, fixture).unwrap();
        let sources = discovery::scan(vec![path.clone()], &CancellationToken::default());
        std::fs::remove_file(path).unwrap();
        let sources = sources.unwrap();
        assert!(sources[0].supported, "{}", sources[0].issue);
    }

    #[test]
    fn separate_extracted_folders_join_their_dump_without_adding_unselected_content() {
        use super::{cli, CancellationToken, IntegrityPolicy, WriterOptions};
        use nxformat_packer::import::{
            structures::NpdmProfile,
            synthetic::{npdm, nso},
        };
        use nxformat_packer::{inspect_archive, SnapshotType};

        const ID: u64 = 0x0400_0000_0001_0000;
        let dir = std::env::temp_dir().join(format!("nxconvert-split-dump-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let root = dir.join("Synthetic Game");
        let base = root.join("base");
        let update = root.join("updates/65536");
        let unselected = root.join("updates/131072");
        let dlc = root.join(format!("dlc/{:016x}/0", ID + 0x1001));
        for (path, build) in [(&base, 0x41), (&update, 0x42), (&unselected, 0x43)] {
            std::fs::create_dir_all(path.join("exefs")).unwrap();
            std::fs::write(
                path.join("exefs/main.npdm"),
                npdm(ID, NpdmProfile::MetaAcid3072V1),
            )
            .unwrap();
            std::fs::write(path.join("exefs/main"), nso(build, true)).unwrap();
        }
        std::fs::create_dir_all(dlc.join("romfs")).unwrap();
        std::fs::write(dlc.join("romfs/addon.bin"), b"addon").unwrap();
        let mut app = App::default();
        let ctx = super::egui::Context::default();
        let scan = |app: &mut App, paths| {
            app.scan(paths);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while app.busy() {
                app.poll(&ctx);
                assert!(
                    std::time::Instant::now() < deadline,
                    "source scan timed out"
                );
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        scan(&mut app, vec![update]);
        assert!(!app.sources[0].supported);
        scan(&mut app, vec![base]);
        scan(&mut app, vec![dlc]);
        assert_eq!(app.sources.len(), 1);
        assert_eq!(app.sources[0].path, root);
        assert_eq!(app.sources[0].tree_roots.len(), 3);
        let output = app.games().remove(0).output.unwrap();
        app.convert();
        let job = app.queue.pop_front().unwrap();
        cli::run_with_display(
            job.args,
            None,
            Some(CancellationToken::default()),
            job.display,
        )
        .unwrap();
        let archive =
            inspect_archive(output, &WriterOptions::default(), IntegrityPolicy::Strict).unwrap();
        let snapshots = &archive.applications[0].snapshots;
        assert_eq!(snapshots.len(), 3);
        assert!(snapshots
            .iter()
            .any(|s| s.snapshot_type == SnapshotType::Dlc));
        assert!(snapshots.iter().any(|s| s.numeric_version == 65536));
        assert!(!snapshots.iter().any(|s| s.numeric_version == 131072));
        std::fs::remove_dir_all(dir).unwrap();
    }
}

fn bytes(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
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
    kind: SourceKind,
    missing_title: bool,
    application: u64,
    name: String,
    default_title: String,
    publisher: String,
    sources: Vec<usize>,
    output: Option<PathBuf>,
    problem: String,
}

#[derive(Default)]
struct Appearance {
    title: String,
    publisher: String,
    icon: Option<Arc<[u8]>>,
    texture: Option<egui::TextureHandle>,
}

type EncodedArtwork = Result<(Vec<u8>, egui::ColorImage), String>;

struct App {
    appearances: HashMap<String, Appearance>,
    textures: HashMap<PathBuf, egui::TextureHandle>,
    archive_cover: Option<egui::TextureHandle>,
    focused: String,
    crop: Option<artwork::Crop>,
    art_rx: Option<mpsc::Receiver<(String, Result<artwork::Artwork, String>)>>,
    crop_rx: Option<mpsc::Receiver<(String, EncodedArtwork)>>,
    last_output: Option<PathBuf>,
    remove_confirmation: Option<(String, Vec<String>)>,
    sources: Vec<Source>,
    output_dir: Option<PathBuf>,
    level: i32,
    no_compress: bool,
    recipe: bool,
    include_firmware: bool,
    omitted_firmware: HashSet<String>,
    nup_platforms: HashMap<PathBuf, Platform>,
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
            crop_rx: None,
            last_output: None,
            remove_confirmation: None,
            sources: Vec::new(),
            output_dir: None,
            level: 1,
            no_compress: false,
            recipe: true,
            include_firmware: true,
            omitted_firmware: HashSet::new(),
            nup_platforms: HashMap::new(),
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
            metadata: None,
            source_bytes: None,
        });
    }

    fn scan(&mut self, paths: Vec<PathBuf>) {
        let mut fresh: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| {
                !self
                    .sources
                    .iter()
                    .any(|s| &s.path == path && s.tree_roots.is_empty())
            })
            .collect();
        if fresh.is_empty() {
            return;
        }
        if fresh.iter().any(|p| p.is_dir()) {
            // Reapply earlier component selections when the same dump is added to.
            fresh.extend(
                self.sources
                    .iter()
                    .filter(|source| source.kind == SourceKind::Extracted)
                    .flat_map(|source| source.tree_roots.iter().map(|root| source.path.join(root))),
            );
        }
        self.archive = None;
        let mut args = vec!["scan".to_string()];
        args.extend(fresh.iter().map(|p| text(p)));
        self.push("Reading the files", args, Purpose::Scan);
    }

    fn open_archive(&mut self, path: PathBuf) {
        self.archive_cover = None;
        self.appearances.remove(ARCHIVE_ARTWORK);
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
        if self.busy() || self.art_rx.is_some() || self.crop.is_some() || self.crop_rx.is_some() {
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
            let key = if self
                .archive
                .as_ref()
                .and_then(|archive| archive.metadata.as_ref())
                .is_some_and(|metadata| metadata.editable)
            {
                ARCHIVE_ARTWORK.into()
            } else if self.archive.is_none() {
                self.focused.clone()
            } else {
                String::new()
            };
            if !key.is_empty() {
                self.load_artwork(key, path.clone());
            } else {
                self.status =
                    "Select a source or open the metadata editor before choosing artwork.".into();
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
            let Some(first) = source
                .contents
                .iter()
                .find(|c| c.kind == "base" || c.kind == "standalone")
                .or_else(|| source.contents.first())
            else {
                continue;
            };
            let key = if source.kind == SourceKind::Native {
                format!("{}:{:016X}", first.platform, first.application)
            } else {
                text(&source.path)
            };
            let at = match games.iter().position(|g| g.key == key) {
                Some(at) => at,
                None => {
                    games.push(Game {
                        key,
                        kind: source.kind,
                        missing_title: true,
                        application: first.application,
                        name: String::new(),
                        default_title: String::new(),
                        publisher: String::new(),
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
                    game.publisher = c.publisher.clone();
                }
            }
        }
        for game in &mut games {
            game.missing_title = game.name.is_empty();
            if game.name.is_empty() {
                game.name = game
                    .sources
                    .first()
                    .map(|&i| title_from_path(&self.sources[i].path))
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| "Untitled game".into());
            }
            game.default_title = game.name.clone();
            if let Some(appearance) = self.appearances.get(&game.key) {
                if !appearance.title.trim().is_empty() {
                    game.name = appearance.title.trim().into();
                }
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
            if game.kind == SourceKind::Nup && self.nup_platform(bases[0]).is_none() {
                game.problem = "Select Switch 1 or Switch 2 for this NUP.".into();
                continue;
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
        game.kind == SourceKind::Native
            && game.sources.iter().any(|&index| {
                let source = &self.sources[index];
                source.selected && source.supported && source.kind == SourceKind::Native
            })
    }

    fn firmware_bytes(&self, game: &Game) -> u64 {
        game.sources
            .iter()
            .map(|&index| &self.sources[index])
            .filter(|source| {
                source.selected && source.supported && source.kind == SourceKind::Native
            })
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
            self.nup_platforms.remove(&source.path);
        }
        self.appearances.remove(&game.key);
        self.omitted_firmware.remove(&game.key);
        if self.focused == game.key {
            self.focused.clear();
        }
    }

    fn nup_platform(&self, source: &Source) -> Option<Platform> {
        source
            .nup_platform
            .or_else(|| self.nup_platforms.get(&source.path).copied())
            .filter(|platform| *platform != Platform::Unknown)
    }

    fn convert(&mut self) {
        for game in self.games() {
            let Some(output) = &game.output else {
                continue;
            };
            let selected: Vec<_> = game
                .sources
                .iter()
                .map(|&i| &self.sources[i])
                .filter(|s| s.selected && s.supported)
                .collect();
            let source_bytes = selected
                .iter()
                .map(|s| s.size)
                .fold(0u64, u64::saturating_add);
            let command = match game.kind {
                SourceKind::Native => "pack",
                SourceKind::Extracted => "pack-tree",
                SourceKind::Nup => "pack-nup",
            };
            let mut args = vec![command.to_owned()];
            args.extend(selected.iter().map(|s| text(&s.path)));
            args.push(text(output));
            if self.no_compress {
                args.push("--no-compress".into());
            } else {
                args.push("--level".into());
                args.push(self.level.to_string());
            }
            if game.kind == SourceKind::Native {
                let omit_firmware = self.omits_firmware(&game);
                if !self.recipe || omit_firmware {
                    args.push("--no-recipe".into());
                }
                if omit_firmware {
                    args.push("--omit-firmware".into());
                }
            }
            if game.kind == SourceKind::Nup {
                let platform = match selected.first().and_then(|s| self.nup_platform(s)) {
                    Some(Platform::Switch1) => "switch1",
                    Some(Platform::Switch2) => "switch2",
                    _ => continue,
                };
                args.extend(["--platform".into(), platform.into()]);
            }
            if game.kind == SourceKind::Extracted {
                for root in &selected[0].tree_roots {
                    args.extend(["--tree-root".into(), root.clone()]);
                }
            }
            if game.kind != SourceKind::Native {
                // One NX file only: no review files beside it.
                args.push("--no-trail".into());
            }
            args.push("--frontend".into());
            self.push(format!("Converting {}", game.name), args, Purpose::Pack);
            let job = self.queue.back_mut().unwrap();
            job.source_bytes = (source_bytes != 0).then_some(source_bytes);
            let appearance = self.appearances.get(&game.key);
            let title = appearance
                .map(|a| a.title.trim())
                .filter(|t| !t.is_empty() && *t != game.default_title)
                .or((game.missing_title && game.name != "Untitled game").then_some(&*game.name))
                .map(String::from);
            let publisher = appearance
                .map(|a| a.publisher.trim())
                .filter(|p| !p.is_empty() && *p != game.publisher)
                .map(String::from);
            let icon = appearance
                .and_then(|a| a.icon.clone())
                .map(|b| (nxformat_packer::MediaType::JpegIcon, b));
            if title.is_some() || publisher.is_some() || icon.is_some() {
                job.display = Some(nxformat_packer::DisplayFallback {
                    title,
                    publisher,
                    icon,
                    replace_existing: true,
                    application_id: Some(game.application),
                });
            }
        }
    }

    fn edit(&mut self, label: String, mut args: Vec<String>) {
        args.push("--frontend".into());
        self.push(label, args, Purpose::Edit);
    }

    fn add_scanned_sources(&mut self, sources: Vec<Source>, ctx: &egui::Context) {
        for mut source in sources {
            let existing = self.sources.iter().position(|s| s.path == source.path);
            let mut previous_defaults = None;
            if let Some(index) = existing {
                let previous = &self.sources[index];
                if previous.tree_roots.is_empty() || previous.tree_roots == source.tree_roots {
                    continue;
                }
                if previous.supported && source.supported {
                    source.selected = previous.selected;
                }
                let key = text(&source.path);
                if self.appearances.contains_key(&key) {
                    previous_defaults = self.games().into_iter().find(|game| game.key == key);
                }
                self.textures.remove(&source.path);
            }
            if let Some(bytes) = source.icon.take() {
                if let Ok(mut art) = artwork::Artwork::decode(&bytes) {
                    self.textures
                        .insert(source.path.clone(), art.texture(ctx).clone());
                }
                source.icon = Some(bytes);
            }
            if let Some(index) = existing {
                self.sources[index] = source;
            } else {
                self.sources.push(source);
            }
            if let Some(previous) = previous_defaults {
                if let Some(updated) = self
                    .games()
                    .into_iter()
                    .find(|game| game.key == previous.key)
                {
                    if let Some(draft) = self.appearances.get_mut(&previous.key) {
                        if draft.title == previous.default_title {
                            draft.title = updated.default_title;
                        }
                        if draft.publisher == previous.publisher {
                            draft.publisher = updated.publisher;
                        }
                    }
                }
            }
        }
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
                            size: artwork::DEFAULT_SIZE,
                        })
                    }
                    Err(error) => self.status = error,
                }
            }
        }
        if let Some(rx) = &self.crop_rx {
            match rx.try_recv() {
                Ok((key, result)) => {
                    self.crop_rx = None;
                    match result {
                        Ok((bytes, preview)) => {
                            let appearance = self.appearances.entry(key).or_default();
                            appearance.texture = Some(ctx.load_texture(
                                "cropped artwork",
                                preview,
                                egui::TextureOptions::LINEAR,
                            ));
                            appearance.icon = Some(Arc::from(bytes));
                        }
                        Err(error) => self.status = error,
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.crop_rx = None;
                    self.status = "Artwork processing stopped unexpectedly.".into();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let mut finished = None;
        let mut scanned = Vec::new();
        while let Ok(message) = running.rx.try_recv() {
            match message {
                Message::Sources(sources) => scanned.extend(sources),
                Message::Metadata(mut metadata) => {
                    self.archive_cover = metadata
                        .as_ref()
                        .and_then(|m| m.icon.as_deref())
                        .and_then(|bytes| artwork::Artwork::decode(bytes).ok())
                        .map(|mut art| art.texture(ctx).clone());
                    if let Some(m) = &mut metadata {
                        m.original_texture = m
                            .original
                            .as_deref()
                            .and_then(|bytes| artwork::Artwork::decode(bytes).ok())
                            .map(|mut art| art.texture(ctx).clone());
                    }
                    if let Some(archive) = &mut self.archive {
                        archive.metadata = metadata;
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
        self.add_scanned_sources(scanned, ctx);
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
            Purpose::Pack | Purpose::Edit | Purpose::Export => {}
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
            if result.is_ok()
                && running
                    .job
                    .args
                    .first()
                    .is_some_and(|cmd| cmd == "set-metadata")
            {
                self.appearances.remove(ARCHIVE_ARTWORK);
            }
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
                self.nup_platforms.retain(|p, _| !used.contains(&&text(p)));
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
                        native: false,
                        kind: kind.into(),
                        application: u64::from_str_radix(application, 16).unwrap_or(0),
                        title: u64::from_str_radix(title, 16).unwrap_or(0),
                        version: version.parse().unwrap_or(0),
                        display: display.into(),
                        name: name.into(),
                    })
                }
                ["STORAGE", identity, storage, ..] => {
                    if let Some(snapshot) = archive
                        .snapshots
                        .iter_mut()
                        .find(|s| s.identity == *identity)
                    {
                        snapshot.native = storage == "native";
                    }
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

    fn encode_artwork(&mut self, crop: artwork::Crop, ctx: egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.crop_rx = Some(rx);
        std::thread::spawn(move || {
            let result = crop.art.crop(crop.center, crop.zoom, crop.size);
            let _ = tx.send((crop.key, result));
            ctx.request_repaint();
        });
    }
}

mod view;
