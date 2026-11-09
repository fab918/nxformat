//! The nxconvert commands. The `nxconvert` binary runs them from a shell; a
//! frontend runs them in its own process through [`run`], gets each output
//! line from a [`Sink`] and cancels with a [`CancellationToken`].
//!
//! Lines starting with `@` are the machine-readable frontend protocol.

// Every line goes to the caller's sink when one is set, else to stdout/stderr.
macro_rules! println {
    () => { $crate::cli::emit(false, String::new()) };
    ($($t:tt)*) => { $crate::cli::emit(false, format!($($t)*)) };
}
macro_rules! eprintln {
    () => { $crate::cli::emit(true, String::new()) };
    ($($t:tt)*) => { $crate::cli::emit(true, format!($($t)*)) };
}
use crate::conversion::convert_inventory;
use crate::native::{self, inventory, InventoryMode, InventoryOptions};
use crate::recipe;
use crate::{
    import::{self, structures::NpdmProfile},
    *,
};
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

/// Receives each output line, without its line break. `true` marks a
/// diagnostic line (stderr for the binary). Called from more than one thread.
pub type Sink = Arc<dyn Fn(bool, &str) + Send + Sync>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    static CANCEL: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
    static DISPLAY: RefCell<Option<DisplayFallback>> = const { RefCell::new(None) };
}

fn current_sink() -> Option<Sink> {
    SINK.with(|s| s.borrow().clone())
}

#[doc(hidden)]
pub fn emit(error: bool, line: String) {
    match current_sink() {
        Some(sink) => sink(error, &line),
        None if error => {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
        None => {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    }
}

/// Restores the thread's previous sink and token when a run ends.
struct Scope(
    Option<Sink>,
    Option<CancellationToken>,
    Option<DisplayFallback>,
);
impl Drop for Scope {
    fn drop(&mut self) {
        SINK.with(|s| *s.borrow_mut() = self.0.take());
        CANCEL.with(|c| *c.borrow_mut() = self.1.take());
        DISPLAY.with(|d| *d.borrow_mut() = self.2.take());
    }
}

/// Runs one nxconvert command, given its arguments without the program name.
/// With `sink`, output lines go there instead of stdout/stderr. With `cancel`,
/// the command stops at its next check when the token is cancelled and leaves
/// no partial output. With a sink or a token, `--frontend` never reads stdin.
pub fn run(
    args: Vec<String>,
    sink: Option<Sink>,
    cancel: Option<CancellationToken>,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_display(args, sink, cancel, None)
}

/// Runs a command with optional display-only metadata for missing native fields.
pub fn run_with_display(
    args: Vec<String>,
    sink: Option<Sink>,
    cancel: Option<CancellationToken>,
    display: Option<DisplayFallback>,
) -> Result<(), Box<dyn std::error::Error>> {
    // In process, --frontend never reads stdin: the caller's token cancels,
    // or nothing does.
    let cancel = cancel.or_else(|| sink.as_ref().map(|_| CancellationToken::default()));
    let previous = (
        SINK.with(|s| std::mem::replace(&mut *s.borrow_mut(), sink)),
        CANCEL.with(|c| std::mem::replace(&mut *c.borrow_mut(), cancel)),
        DISPLAY.with(|d| std::mem::replace(&mut *d.borrow_mut(), display)),
    );
    let _scope = Scope(previous.0, previous.1, previous.2);
    run_command(args)
}
const MIB: u64 = 1024 * 1024;
/// Hard ceilings accepted by `ImportLimits::validate`.
const NODE_CEILING: usize = 1_000_000;
const INVENTORY_CEILING: u64 = 1024 * MIB;
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn usage() -> &'static str {
    "nxconvert inspect-tree TREE 2048|3072|none\n\
nxconvert inspect-tree PACKAGE MANIFEST KEY\n\
nxconvert pack-tree PACKAGE OUTPUT.nx [--platform switch1|switch2]\n\
nxconvert pack-tree PACKAGE MANIFEST OUTPUT.nx RECEIPT [--latest] [--dlc KEY]\n\
nxconvert pack-nup INPUT.nup OUTPUT.nx --platform switch1|switch2 [--skip-dlc]\n\
nxconvert check-nup INPUT.nup ARCHIVE.nx [--skip-dlc] (every NUP file is in the NX: paths, NACP, and bytes read back and hashed)\n\
nxconvert pack SOURCE... OUTPUT.nx [--no-recipe [--omit-firmware]] (decrypted DNSP/DXCI; keeps exact rebuild recipes by default)\n\
nxconvert recipe ARCHIVE.nx SOURCE|RECIPE.nxrecipe... (keep rebuild recipes inside the NX)\n\
nxconvert rebuild ARCHIVE.nx FOLDER [RECIPE_ID...] (rebuild the original files into FOLDER)\n\
nxconvert check-recipe ARCHIVE.nx (replay every recipe kept in the NX)\n\
nxconvert scan SOURCE|FOLDER... (decrypted packages; @SOURCE, @CONTENT and @ISSUE lines)\n\
nxconvert contents ARCHIVE.nx (@APP, @SNAPSHOT, @RESOURCE, @RECIPE, @RECLAIMABLE and @ALLOCATED lines)\n\
nxconvert add ARCHIVE.nx SOURCE... [--no-recipe [--omit-firmware]] (add updates and DLC in place, with rebuild recipes by default)\n\
nxconvert add-tree ARCHIVE.nx PACKAGE [--platform switch1|switch2] [--update-version N] (add extracted trees in place: updates/<version>/ and dlc/ folders, or with --update-version one update folder holding exefs/ and romfs/)\n\
nxconvert remove ARCHIVE.nx SNAPSHOT_IDENTITY... (remove in place)\n\
nxconvert compact ARCHIVE.nx [--no-compress] (rewrite as RAW to remove existing game-data compression)\n\
nxconvert set-metadata ARCHIVE.nx APPLICATION_ID [title=TEXT] [publisher=TEXT] [icon=FILE.jpg|png] (in place; replaces the localized titles and icons)\n\
--frontend (pack, pack-nup, pack-tree, add, add-tree, remove, compact, rebuild): @TOTAL and @PROGRESS lines (add the @TOTAL values); closing stdin cancels and leaves no partial file\n\
--omit-firmware (native pack/add, requires --no-recipe): skip newly imported DXCI firmware; existing archive resources are preserved\n\
--no-trail (pack-nup, automatic pack-tree, add-tree): write only the archive, without the .nximport, .assumptions.txt and .nxreceipt review files\n\
nxconvert info ARCHIVE.nx\n\
nxconvert verify ARCHIVE.nx [PACKAGE RECEIPT]\n\
Display fallbacks (conversion): --fallback-title TITLE --fallback-icon JPEG_OR_PNG (missing fields only)\n\
Budget options (any command): --memory-mib N --temp-mib N --max-nodes N --inventory-mib N --no-compress (store new data blocks as RAW; overrides --level) --min-gain-bp N (keep Zstd only if it saves N/100 %) --compressed-chunk-kib 0|64|128|256 (chunk for files that compress, 64 by default, 0 = off) --workers N (0 = automatic, the default) --level 1-13 (Zstd level, 3 by default) --paranoid (re-read and compare all source bytes)\n\
Without explicit budgets, pack-tree and source verify size the inventory from the tree."
}
#[derive(Default)]
struct Options {
    fallback_title: Option<String>,
    fallback_icon: Option<PathBuf>,
    latest: bool,
    dlc: std::collections::BTreeSet<String>,
    platform: Option<String>,
    memory: Option<u64>,
    temp: Option<u64>,
    max_nodes: Option<usize>,
    inventory: Option<u64>,
    min_gain_bp: Option<u16>,
    compressed_chunk_kib: Option<u32>,
    level: Option<i32>,
    no_compress: bool,
    workers: Option<u8>,
    paranoid: bool,
    skip_dlc: bool,
    /// `--recipe` / `--no-recipe`; pack and add keep recipes unless told not to.
    recipe: Option<bool>,
    omit_firmware: bool,
    /// Progress lines and cancel-on-stdin-close for a GUI frontend.
    frontend: bool,
    /// pack-nup, automatic pack-tree, add-tree: keep no declarations,
    /// assumptions or receipt next to the archive.
    no_trail: bool,
    /// add-tree: the folder is one update tree of this version.
    update_version: Option<u32>,
}
fn parse(args: Vec<String>) -> Result<(Vec<String>, Options), Box<dyn std::error::Error>> {
    let mut positional = Vec::new();
    let mut o = Options::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if !a.starts_with("--") || a == "--help" {
            positional.push(a);
            continue;
        }
        if a == "--latest" {
            o.latest = true;
            continue;
        }
        if a == "--paranoid" {
            o.paranoid = true;
            continue;
        }
        if a == "--no-compress" {
            o.no_compress = true;
            continue;
        }
        if a == "--frontend" {
            o.frontend = true;
            continue;
        }
        if a == "--omit-firmware" {
            o.omit_firmware = true;
            continue;
        }
        if a == "--no-trail" {
            o.no_trail = true;
            continue;
        }
        if a == "--recipe" || a == "--no-recipe" {
            o.recipe = Some(a == "--recipe");
            continue;
        }
        if a == "--skip-dlc" {
            o.skip_dlc = true;
            continue;
        }
        let value = it.next().ok_or("missing option value")?;
        let mib = || -> Result<u64, Box<dyn std::error::Error>> {
            Ok(value
                .parse::<u64>()?
                .checked_mul(MIB)
                .ok_or("budget overflow")?)
        };
        match a.as_str() {
            "--fallback-title" => o.fallback_title = Some(value.clone()),
            "--fallback-icon" => o.fallback_icon = Some(PathBuf::from(&value)),
            "--dlc" => {
                o.dlc.insert(value.clone());
            }
            "--platform" => o.platform = Some(value.clone()),
            "--memory-mib" => o.memory = Some(mib()?),
            "--temp-mib" => o.temp = Some(mib()?),
            "--inventory-mib" => o.inventory = Some(mib()?),
            "--max-nodes" => o.max_nodes = Some(value.parse()?),
            "--min-gain-bp" => o.min_gain_bp = Some(value.parse()?),
            "--compressed-chunk-kib" => o.compressed_chunk_kib = Some(value.parse()?),
            "--workers" => o.workers = Some(value.parse()?),
            "--level" => o.level = Some(value.parse()?),
            "--update-version" => o.update_version = Some(value.parse()?),
            _ => return Err(usage().into()),
        }
    }
    Ok((positional, o))
}
/// Bounded pre-walk: counts nodes, estimates the importer's conservative
/// inventory charge and lists `.listing` artifacts. Symlinks are refused.
struct Walk {
    nodes: usize,
    inventory_estimate: u64,
    listings: Vec<String>,
    /// Sum of the file sizes.
    bytes: u64,
}
fn walk(tree: &Path, cap: usize) -> Result<Walk, Box<dyn std::error::Error>> {
    let mut out = Walk {
        nodes: 0,
        inventory_estimate: 2 * MIB,
        listings: Vec::new(),
        bytes: 0,
    };
    let mut pending = vec![tree.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            out.nodes += 1;
            if out.nodes > cap {
                return Err(format!(
                    "tree has more than {cap} entries; outside the supported import ceiling"
                )
                .into());
            }
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                return Err("source symlinks unsupported".into());
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(tree)?
                .to_str()
                .ok_or("non-UTF8 source path")?
                .replace('\\', "/");
            // Mirrors tree inventory: listing scratch + per-node charge
            // (1024 + 4*logical + 2*source path), with room for the package prefix.
            out.inventory_estimate += 2048 + 6 * relative.len() as u64 + 2 * 256;
            if ty.is_dir() {
                pending.push(path);
            } else if ty.is_file() && entry.file_name() == ".listing" {
                out.listings.push(relative);
            } else if ty.is_file() {
                out.bytes += entry.metadata()?.len();
            }
        }
    }
    Ok(out)
}
/// Derive import/writer budgets from a measured tree unless explicitly set.
fn size_budgets(
    measured: Option<&Walk>,
    o: &Options,
    limits: &mut import::ImportLimits,
    writer: &mut WriterOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    writer.display_fallback = DISPLAY.with(|d| d.borrow().clone());
    if o.fallback_title.is_some() || o.fallback_icon.is_some() {
        use std::io::Read;
        let icon = if let Some(path) = &o.fallback_icon {
            let mut bytes = Vec::new();
            File::open(path)?
                .take(16 * MIB + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > 16 * MIB {
                return Err("fallback icon exceeds 16 MiB".into());
            }
            let kind = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                MediaType::PngIcon
            } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
                MediaType::JpegIcon
            } else {
                return Err("fallback icon must be PNG or JPEG".into());
            };
            Some((kind, Arc::from(bytes)))
        } else {
            None
        };
        writer.display_fallback = Some(DisplayFallback {
            title: o.fallback_title.clone(),
            icon,
        });
    }
    if let Some(w) = measured {
        if o.max_nodes.is_none() {
            limits.max_nodes = limits.max_nodes.max(w.nodes + 4096).min(NODE_CEILING);
        }
        if o.inventory.is_none() {
            let wanted = w.inventory_estimate / 4 * 5 + 16 * MIB;
            limits.max_inventory_bytes = limits
                .max_inventory_bytes
                .max(wanted)
                .min(INVENTORY_CEILING);
        }
        if o.memory.is_none() {
            // Verification charges the inventory against writer memory a second time.
            writer.max_memory_bytes = writer
                .max_memory_bytes
                .max(limits.max_inventory_bytes * 2 + 512 * MIB);
        }
    }
    if let Some(n) = o.max_nodes {
        limits.max_nodes = n;
    }
    if let Some(n) = o.inventory {
        limits.max_inventory_bytes = n;
    }
    if let Some(n) = o.memory {
        writer.max_memory_bytes = n;
    }
    if let Some(n) = o.temp {
        writer.max_temporary_disk_bytes = n;
    }
    limits.compare_source_bytes = o.paranoid;
    if o.no_compress {
        writer.compression_mode = CompressionMode::RawOnly;
    }
    if let Some(n) = o.workers {
        writer.compression_workers = n;
    }
    if let Some(n) = o.level {
        writer.zstd_compression_level = n;
    }
    if let Some(n) = o.min_gain_bp {
        writer.min_compression_savings_bp = n;
    }
    if let Some(n) = o.compressed_chunk_kib {
        writer.compressed_chunk_bytes = n
            .checked_mul(1024)
            .ok_or("--compressed-chunk-kib is too large")?;
    }
    limits.validate()?;
    Ok(())
}
/// `[0400C3F00006E000]`-style hint in a folder name. Hint only, never a source.
fn folder_id_hint(package: &Path) -> Option<u64> {
    let name = package.canonicalize().ok()?;
    let name = name.file_name()?.to_str()?;
    let start = name.find('[')?;
    let inner = &name[start + 1..];
    let end = inner.find(']')?;
    let id = &inner[..end];
    (id.len() == 16)
        .then(|| u64::from_str_radix(id, 16).ok())
        .flatten()
}
/// Platform: explicit declaration wins; otherwise inferred only when the
/// Program ID range and NPDM key profile agree. Anything else needs a declaration.
fn platform_for(
    declared: Option<&str>,
    program: u64,
    profile: NpdmProfile,
) -> Result<(String, &'static str), Box<dyn std::error::Error>> {
    if let Some(p) = declared {
        if p != "switch1" && p != "switch2" {
            return Err("invalid platform declaration; use switch1 or switch2".into());
        }
        return Ok((p.to_string(), "declared by the user"));
    }
    match (program >> 56, profile) {
        (0x04, _) => Ok((
            "switch2".into(),
            "inferred: Program ID in the 04xx Switch 2 title range",
        )),
        (0x01, NpdmProfile::MetaAcid2048V1) => Ok((
            "switch1".into(),
            "inferred: Program ID in the 01xx range with a 2048-bit ACID profile",
        )),
        _ => Err(format!(
            "cannot infer platform for Program ID {program:016x} with {profile:?}; pass --platform switch1|switch2"
        )
        .into()),
    }
}
struct AutoPlan {
    manifest: String,
    receipt: String,
    walk: Walk,
}
/// Program ID from a tree's exefs/main.npdm (ACI0), validated by the importer.
fn observe_program(
    tree: &Path,
    limits: &import::ImportLimits,
) -> Option<(u64, NpdmProfile, &'static str)> {
    for (profile, name) in [
        (NpdmProfile::MetaAcid2048V1, "meta-acid2048-v1"),
        (NpdmProfile::MetaAcid3072V1, "meta-acid3072-v1"),
    ] {
        if let Ok(audit) =
            import::audit_executables(tree, profile, limits, &CancellationToken::default())
        {
            if let Some(npdm) = audit.npdm {
                return Some((npdm.program_id, profile, name));
            }
        }
    }
    None
}
/// What a dump layout directory names: `base` (or the package itself),
/// `updates/<version>`, `dlc/<add-on id>/<version>` or `dlc/<add-on id>`.
#[derive(Clone, Copy, Debug)]
enum Role {
    Base,
    Update(u32),
    Dlc(u64, u32),
}
fn role(root: &str) -> Option<Role> {
    let parts: Vec<_> = root.split('/').collect();
    let hex16 = |s: &str| {
        (s.len() == 16)
            .then(|| u64::from_str_radix(s, 16).ok())
            .flatten()
    };
    match parts.as_slice() {
        ["."] | ["base"] => Some(Role::Base),
        ["updates", v] => v.parse().ok().map(Role::Update),
        ["dlc", id] => hex16(id).map(|id| Role::Dlc(id, 0)),
        ["dlc", id, v] => Some(Role::Dlc(hex16(id)?, v.parse().ok()?)),
        _ => None,
    }
}
// Convenience policy for dumps: one Program, its updates and its add-ons.
// The generated declarations and the assumption record are kept next to
// the archive.
fn automatic_manifest(
    package: &Path,
    output: &str,
    o: &Options,
) -> Result<AutoPlan, Box<dyn std::error::Error>> {
    let limits = import::ImportLimits {
        max_nodes: o.max_nodes.unwrap_or(NODE_CEILING),
        max_inventory_bytes: o.inventory.unwrap_or(INVENTORY_CEILING),
        ..Default::default()
    };
    limits.validate()?;
    let candidates = import::discover(package, &limits)?;
    let mut trees = Vec::new();
    for c in &candidates {
        let r = role(&c.relative_root).ok_or_else(|| {
            format!(
                "unknown layout directory {}; use an explicit manifest",
                c.relative_root
            )
        })?;
        let r = match (r, o.update_version) {
            (Role::Base, Some(v)) if c.relative_root == "." => Role::Update(v),
            (r, _) => r,
        };
        trees.push((c.relative_root.clone(), r));
    }
    let bases = trees.iter().filter(|t| matches!(t.1, Role::Base)).count();
    let updates = trees
        .iter()
        .filter(|t| matches!(t.1, Role::Update(..)))
        .count();
    // Without a base, one update carries the whole game (a standalone update,
    // as in a dump whose base folder is empty); its DLC may sit beside it.
    let set = bases == 1;
    if trees.is_empty() || bases > 1 || (bases == 0 && updates != 1) {
        let roots: Vec<_> = trees.iter().map(|t| t.0.as_str()).collect();
        return Err(format!(
            "automatic mode needs one base (with its updates and DLC) or one standalone update, found {roots:?}; use an explicit manifest"
        )
        .into());
    }
    let path_of = |root: &str| {
        if root == "." {
            package.to_path_buf()
        } else {
            package.join(root)
        }
    };
    // The application's Program: from the base, or the standalone update.
    let main_root = trees
        .iter()
        .find(|t| !matches!(t.1, Role::Dlc(..)))
        .map(|t| t.0.clone())
        .ok_or("DLC alone cannot name its application; use an explicit manifest")?;
    let (program, profile, profile_name) = observe_program(&path_of(&main_root), &limits)
        .ok_or("no supported validated exefs/main.npdm; explicit manifest required")?;
    eprintln!(
        "OBSERVED Program ID {program:016x} from {main_root}/exefs/main.npdm ({profile_name})"
    );
    let hint = folder_id_hint(package);
    if let Some(h) = hint {
        if h != program {
            return Err(format!(
                "folder name declares {h:016x} but main.npdm has {program:016x}; explicit manifest required"
            )
            .into());
        }
    }
    let env = std::env::var("NX_TREE_PLATFORM").ok();
    let (platform, platform_origin) =
        platform_for(o.platform.as_deref().or(env.as_deref()), program, profile)?;
    let walk = walk(package, limits.max_nodes)?;
    let mut text = "NXIMPORT\t1\n".to_owned();
    let mut facts = String::new();
    let mut keys = Vec::new();
    trees.sort_by_key(|t| match t.1 {
        Role::Base => (0, 0, 0),
        Role::Update(v) => (1, 0, v),
        Role::Dlc(id, v) => (2, id, v),
    });
    for (root, r) in &trees {
        let key = match r {
            Role::Base => "game".to_owned(),
            Role::Update(v) => format!("update-{v}"),
            Role::Dlc(id, v) => format!("dlc-{id:016x}-{v}"),
        };
        match *r {
            Role::Base => text.push_str(&format!(
                "snapshot\t{key}\t{root}\t{platform}\t{program:016x}\t{program:016x}\tprimary\tbase\t0\tcomplete\tmaterialized-v1\t{profile_name}\n"
            )),
            Role::Update(v) => {
                let (own, own_profile, own_name) = observe_program(&path_of(root), &limits)
                    .ok_or_else(|| format!("{root}: no supported validated exefs/main.npdm"))?;
                if own != program || own_profile != profile {
                    return Err(format!(
                        "{root}/exefs/main.npdm has {own:016x} ({own_name}), not {program:016x}; explicit manifest required"
                    )
                    .into());
                }
                let kind = if set { "update" } else { "standalone-update" };
                text.push_str(&format!(
                    "snapshot\t{key}\t{root}\t{platform}\t{program:016x}\t{program:016x}\tprimary\t{kind}\t{v}\tcomplete\tmaterialized-v1\t{own_name}\n"
                ));
                if set {
                    text.push_str(&format!("dependency\t{key}\trequired-base\tgame\n"));
                    facts.push_str(&format!(
                        "OBSERVED: {root}/exefs/main.npdm has the same Program ID.\n"
                    ));
                }
                facts.push_str(&format!(
                    "PRESUMED: {root} is a complete patched tree, version {v} from its directory name.\n"
                ));
            }
            Role::Dlc(id, v) => {
                if !import::addon_belongs_to(id, program) {
                    return Err(format!(
                        "{root}: add-on {id:016x} is not in the add-on range of {program:016x}; explicit manifest required"
                    )
                    .into());
                }
                if path_of(root).join("exefs").exists() {
                    return Err(format!("{root}: executable DLC needs an explicit manifest").into());
                }
                text.push_str(&format!(
                    "snapshot\t{key}\t{root}\t{platform}\t{program:016x}\t-\tnone\tdlc\t{v}\tcomplete\tmaterialized-v1\tnone\n"
                ));
                text.push_str(&format!("addon\t{key}\t{id:016x}\n"));
                facts.push_str(&format!(
                    "PRESUMED: {root} is add-on {id:016x} version {v} (directory names), stored by its add-on title ID (no CNMT, no native Content ID).\n"
                ));
            }
        }
        if !matches!(r, Role::Dlc(..)) {
            // Dump layout: control.nacp and icon_<Language>.dat beside exefs/.
            let at = |name: &str| {
                if root == "." {
                    name.to_owned()
                } else {
                    format!("{root}/{name}")
                }
            };
            let dir = path_of(root);
            if fs::metadata(dir.join("control.nacp"))
                .is_ok_and(|m| m.is_file() && m.len() == 0x4000)
            {
                text.push_str(&format!("nacp\t{key}\t{}\n", at("control.nacp")));
                facts.push_str(&format!(
                    "OBSERVED: {} is the title metadata (0x4000 bytes).\n",
                    at("control.nacp")
                ));
                for (language, _) in crate::native::NACP_LANGUAGE_MAP {
                    let name = format!("icon_{language}.dat");
                    if dir.join(&name).is_file() {
                        text.push_str(&format!("icon\t{key}\t{language}\t{}\n", at(&name)));
                    }
                }
            }
        }
        keys.push((root.clone(), key));
    }
    // `.listing` artifacts (FTP directory listings left by the download tool)
    // are excluded from the snapshot that contains them. One outside every
    // content tree (in the top folder or in updates/) belongs to no
    // snapshot and is simply not read.
    for relative in &walk.listings {
        let Some((root, key)) = keys
            .iter()
            .filter(|(root, _)| root == "." || relative.starts_with(&format!("{root}/")))
            .max_by_key(|(root, _)| root.len())
        else {
            continue;
        };
        let inner = if root == "." {
            relative.as_str()
        } else {
            &relative[root.len() + 1..]
        };
        text.push_str(&format!("exclude\t{key}\t{inner}\n"));
    }
    if text.len() as u64 > 16 * MIB {
        return Err("automatic manifest exceeds budget".into());
    }
    let manifest_path = format!("{output}.nximport");
    let assumption_path = format!("{output}.assumptions.txt");
    let parent = Path::new(output)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if parent.canonicalize()?.starts_with(package.canonicalize()?) {
        return Err("output must be outside source package".into());
    }
    let hint_note = match hint {
        Some(h) => format!("Folder name hint {h:016x} agrees (secondary, not a source).\n"),
        None => String::new(),
    };
    let main_note = if matches!(trees[0].1, Role::Base) {
        "PRESUMED: base version 0 and complete (caller selected automatic mode); no composition.\n"
    } else {
        "PRESUMED: standalone update complete (caller selected automatic mode); no composition.\n"
    };
    // Observed facts vs. presumed policy are kept on separate lines.
    let notes = format!(
        "Automatic convenience policy, not native application verification.\n\
OBSERVED: Program ID {program:016x} from {main_root}/exefs/main.npdm, validated with {profile_name}.\n\
PRESUMED: Application ID equal to the sole observed Program ID {program:016x} (no CNMT available).\n\
{hint_note}PLATFORM: {platform} ({platform_origin}).\n\
{main_note}{facts}Budgets: {} entries, inventory estimate {} MiB.\n\
If any assumption is wrong, use an explicit manifest.\n",
        walk.nodes,
        walk.inventory_estimate / MIB
    );
    let mut note = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&assumption_path)?;
    note.write_all(notes.as_bytes())?;
    note.sync_all()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    if o.no_trail {
        eprintln!("PRESUMED application ID/version/completeness (no review files: --no-trail)");
    } else {
        eprintln!("PRESUMED application ID/version/completeness: review {assumption_path}; declarations: {manifest_path}");
    }
    Ok(AutoPlan {
        manifest: manifest_path,
        receipt: format!("{output}.nxreceipt"),
        walk,
    })
}
/// NUP -> NX: decode beside the output, size budgets from the staged tree,
/// then the ordinary extracted-tree conversion.
fn check_nup(args: &[String], o: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 3 {
        return Err(usage().into());
    }
    let started = Instant::now();
    let options = WriterOptions {
        cancellation: command_token(o),
        ..WriterOptions::default()
    };
    let watch = ProgressWatch::counter(o.frontend, &options.progress);
    let limits = import::ImportLimits {
        max_nodes: o.max_nodes.unwrap_or(NODE_CEILING),
        max_inventory_bytes: o.inventory.unwrap_or(INVENTORY_CEILING),
        ..Default::default()
    };
    let check = import::check_nup(
        Path::new(&args[1]),
        Path::new(&args[2]),
        o.skip_dlc,
        &limits,
        &options,
    )?;
    drop(watch);
    println!(
        "PASS snapshots={} files={} files_read={} bytes_verified={} nup_files_read={} nacp={} elapsed_s={:.3}",
        check.snapshots,
        check.files,
        check.files_read,
        check.bytes_verified,
        check.nup_files_read,
        check.nacp_checked,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
fn pack_nup(args: &[String], o: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 3 || o.latest || !o.dlc.is_empty() {
        return Err(usage().into());
    }
    let token = command_token(o);
    let started = Instant::now();
    let output = Path::new(&args[2]);
    if output.exists() {
        return Err("output already exists".into());
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut limits = import::ImportLimits {
        max_nodes: o.max_nodes.unwrap_or(NODE_CEILING),
        max_inventory_bytes: o.inventory.unwrap_or(INVENTORY_CEILING),
        ..Default::default()
    };
    let platform = match o.platform.as_deref() {
        Some("switch1") => Platform::Switch1,
        Some("switch2") => Platform::Switch2,
        _ => return Err("pack-nup requires --platform switch1|switch2".into()),
    };
    let mut options = WriterOptions {
        cancellation: token,
        ..WriterOptions::default()
    };
    let nup_options = import::NupOptions {
        platform: Some(platform),
        skip_addons: o.skip_dlc,
        progress: options.progress.clone(),
        // RomFS is read from the NUP while writing: one pass, one bar.
        stream: true,
    };
    size_budgets(None, o, &mut limits, &mut options)?;
    let watch = ProgressWatch::counter(o.frontend, &options.progress);
    let stage = import::stage_nup_with_options(
        Path::new(&args[1]),
        parent,
        &nup_options,
        &limits,
        &options,
    )?;
    let decoded = started.elapsed();
    for a in &stage.skipped_addons {
        eprintln!("SKIPPED add-on {:016x} v{}", a.title_id, a.version);
    }
    // Same review trail as automatic pack-tree: the generated declarations
    // and what the NUP profile presumes, kept next to the archive.
    let manifest_path = format!("{}.nximport", output.display());
    let notes_path = format!("{}.assumptions.txt", output.display());
    let notes = format!("NUP profile v1 (a55uka/nup SPEC 1.0-draft), not native verification.\n\
OBSERVED: TOC SHA-256, every file size and SHA-256, NPDM/NSO structure and Build IDs.\n\
DECLARED: platform {platform:?} (from --platform).\n\
PRESUMED: application ID = base title ID; an update package carries the application ID or its patch ID (+800).\n\
PRESUMED: an add-on title ID is the application ID + 0x1000 + its index; DLC is stored by that ID (no native Content ID).\n\
PRESUMED: update tree = base RomFS shadowed by the update paths; the update ExeFS replaces the base ExeFS.\n\
PRESUMED: a standalone_update package is complete.\n\
PRESUMED: control.nacp and icons from the package meta TLVs are the native ones.\n\
If any assumption is wrong, extract the NUP and use an explicit manifest with pack-tree.\n");
    if o.no_trail {
        eprintln!("PRESUMED NUP profile (no review files: --no-trail)");
    } else {
        for (path, bytes) in [
            (&manifest_path, stage.manifest.bytes()),
            (&notes_path, notes.as_bytes()),
        ] {
            let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        eprintln!("PRESUMED NUP profile: review {notes_path}; declarations: {manifest_path}");
    }
    limits = import::ImportLimits::default();
    let mut measured = walk(&stage.package(), NODE_CEILING)?;
    // Streamed files are not on disk: count them as walk would.
    for (path, file) in stage.streamed_files() {
        measured.nodes += 1;
        measured.inventory_estimate += 2048 + 6 * (path.len() as u64 + 32) + 2 * 256;
        measured.bytes += file;
    }
    if measured.nodes > NODE_CEILING {
        return Err(format!(
            "tree has more than {NODE_CEILING} entries; outside the supported import ceiling"
        )
        .into());
    }
    size_budgets(Some(&measured), o, &mut limits, &mut options)?;
    let options = stage.remaining_writer_options(&options)?;
    let result = import::convert_extracted_external(
        &stage.package(),
        &stage.manifest,
        output,
        &limits,
        &options,
        stage.external(),
    )?;
    drop(watch);
    println!(
        "PASS nup_decode_s={:.3} conversion_s={:.3} staged_bytes={} nx_bytes={} skipped_addons={} peak_temporary_bytes={}",
        decoded.as_secs_f64(),
        result.elapsed.as_secs_f64(),
        stage.staged_bytes,
        fs::metadata(output)?.len(),
        stage.skipped_addons.len(),
        stage.staged_bytes + result.peak_temporary_disk_bytes
    );
    Ok(())
}
/// The cancellation token of an in-process caller, if any.
fn caller_token() -> Option<crate::CancellationToken> {
    CANCEL.with(|c| c.borrow().clone())
}
/// Extracted tree -> existing NX, in place: one standalone update (for
/// example a Switch 2 update folder), or a base with updates and DLC, gets
/// declarations as automatic pack-tree does and becomes new snapshots of the
/// archive. The declarations, assumptions and receipt are kept next to it.
fn add_tree(args: &[String], o: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() != 3 || o.latest || !o.dlc.is_empty() || o.skip_dlc || o.recipe.is_some() {
        return Err(usage().into());
    }
    if o.update_version == Some(0) {
        return Err("an update version is above 0".into());
    }
    let archive = Path::new(&args[1]);
    let package = Path::new(&args[2]);
    if !archive.is_file() {
        return Err("archive does not exist".into());
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let trail = format!("{}.added-{stamp}", archive.display());
    let plan = automatic_manifest(package, &trail, o)?;
    // A refused or cancelled addition leaves no review trail behind.
    let notes = format!("{trail}.assumptions.txt");
    let discard = |plan: &AutoPlan| {
        for path in [&plan.manifest, &notes, &plan.receipt] {
            let _ = fs::remove_file(path);
        }
    };
    // --no-trail: the review files go away once the addition is done.
    let _trail = RemoveOnDrop(if o.no_trail {
        [&plan.manifest, &notes, &plan.receipt]
            .iter()
            .map(PathBuf::from)
            .collect()
    } else {
        Vec::new()
    });
    let mut limits = import::ImportLimits::default();
    let mut options = WriterOptions {
        cancellation: command_token(o),
        ..WriterOptions::default()
    };
    let _watch = ProgressWatch::counter(o.frontend, &options.progress);
    let attempt = (|| -> Result<_, Box<dyn std::error::Error>> {
        size_budgets(Some(&plan.walk), o, &mut limits, &mut options)?;
        let manifest = import::DeclarationManifest::read(File::open(&plan.manifest)?, &limits)?;
        let receipt_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&plan.receipt)?;
        let result = import::add_extracted(archive, package, &manifest, &limits, &options);
        Ok((manifest, receipt_file, result))
    })();
    let (manifest, mut receipt_file, result) = match attempt {
        Ok((m, f, Ok(r))) => (m, f, r),
        Ok((_, f, Err(e))) => {
            drop(f);
            discard(&plan);
            return Err(e.into());
        }
        Err(e) => {
            discard(&plan);
            return Err(e);
        }
    };
    receipt_file.write_all(&result.receipt.encode(&limits)?)?;
    receipt_file.sync_all()?;
    for d in &manifest.snapshots {
        println!(
            "@ADDED\t{}\t{:016x}\t{}",
            match d.snapshot_type {
                SnapshotType::Base => "base",
                SnapshotType::Dlc => "dlc",
                _ => "update",
            },
            d.application_id,
            d.numeric_version
        );
    }
    println!(
        "PASS added={} compared_bytes={} conversion_s={:.6} nx_bytes={} receipt={}",
        manifest.snapshots.len(),
        result.source_bytes_compared,
        result.elapsed.as_secs_f64(),
        fs::metadata(archive)?.len(),
        plan.receipt
    );
    Ok(())
}
fn print_recipe(path: &Path, s: &recipe::RecipeStats) {
    println!(
        "RECIPE {} kept in the NX: recipe_bytes={} source_bytes={} from_nx={} fill={} literal={} segments={}",
        path.display(),
        s.recipe_bytes,
        s.source_bytes,
        s.referenced_bytes,
        s.fill_bytes,
        s.literal_bytes,
        s.segments
    );
    for (name, size) in &s.unmatched_entries {
        eprintln!("NOT IN NX (kept in the recipe): {name} {size} bytes");
    }
    if s.literal_bytes > 16 * MIB {
        eprintln!(
            "NOTE: {} MiB are not in the NX (for example a gamecard system update) and are kept in the recipe",
            s.literal_bytes / MIB
        );
    }
}
/// One field of a `@` line for frontends: tabs and line breaks never appear.
fn field(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}
/// Name (first non-empty language, American English first) and display
/// version of a control.nacp.
fn nacp_texts(nacp: &[u8]) -> (String, String) {
    let text = |bytes: &[u8]| {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).to_string()
    };
    if nacp.len() < 0x4000 {
        return (String::new(), String::new());
    }
    let name = (0..16)
        .map(|i| text(&nacp[i * 0x300..i * 0x300 + 0x200]))
        .find(|n| !n.trim().is_empty())
        .unwrap_or_default();
    (field(&name), field(&text(&nacp[0x3060..0x3070])))
}
fn meta_kind(t: native::ContentMetaType) -> Option<&'static str> {
    match t {
        native::ContentMetaType::Application => Some("base"),
        native::ContentMetaType::Patch => Some("update"),
        native::ContentMetaType::AddOnContent => Some("dlc"),
        _ => None,
    }
}
/// Machine-readable inventory of decrypted packages: one `@SOURCE` line per
/// file, `@CONTENT` per base/update/DLC, `@ISSUE` per error.
fn print_scan(inv: &native::Inventory) {
    for s in &inv.sources {
        println!(
            "@SOURCE\t{}\t{}\t{:?}\t{:?}",
            field(&s.path.display().to_string()),
            s.size,
            s.format,
            s.status
        );
        for m in &s.metas {
            let Some(kind) = meta_kind(m.meta.meta_type) else {
                continue;
            };
            let (name, display) = m
                .meta
                .control
                .as_ref()
                .map(|c| nacp_texts(&c.nacp_bytes))
                .unwrap_or_default();
            println!(
                "@CONTENT\t{}\t{kind}\t{:016x}\t{:016x}\t{}\t{display}\t{name}",
                field(&s.path.display().to_string()),
                m.meta.title_id,
                m.meta.application_id,
                m.meta.version
            );
        }
    }
    for issue in inv.all_issues() {
        if issue.severity == native::Severity::Error {
            let path = issue
                .source
                .and_then(|i| inv.sources.get(i))
                .map(|s| s.path.display().to_string())
                .unwrap_or_default();
            println!("@ISSUE\t{}\t{}", field(&path), field(&issue.message));
        }
    }
}
/// A native DLC snapshot keeps its title ID in the Meta NCA header (0x210).
fn dlc_title(path: &Path, s: &SnapshotSummary, options: &WriterOptions) -> u64 {
    if let Some(id) = s.addon_title_id.or(s.program_id) {
        return id;
    }
    let Ok(files) = inspect_snapshot_files(path, s.identity, options, IntegrityPolicy::Fast) else {
        return 0;
    };
    files
        .iter()
        .filter(|f| f.path.ends_with(".cnmt.nca") && f.size >= 0x400)
        .find_map(|f| {
            let mut header = [0u8; 0x220];
            read_snapshot_file_range(path, s.identity, &f.path, 0, &mut header, options).ok()?;
            (&header[0x200..0x203] == b"NCA" || &header[0x200..0x204] == b"DNCA")
                .then(|| u64::from_le_bytes(header[0x210..0x218].try_into().unwrap()))
        })
        .unwrap_or(0)
}
/// Machine-readable contents of an NX archive.
fn print_contents(path: &Path, options: &WriterOptions) -> Result<(), Box<dyn std::error::Error>> {
    let (summary, reclaimable, allocated) =
        crate::writer::inspect_archive_with_space(path, options)?;
    for a in &summary.applications {
        println!("@APP\t{:016x}\t{:?}", a.application_id, a.platform);
        for s in &a.snapshots {
            let kind = match s.snapshot_type {
                SnapshotType::Base => "base",
                SnapshotType::Update | SnapshotType::StandaloneUpdate => "update",
                SnapshotType::Dlc => "dlc",
                _ => "other",
            };
            let title = match s.snapshot_type {
                SnapshotType::Dlc => dlc_title(path, s, options),
                SnapshotType::Update | SnapshotType::StandaloneUpdate => a.application_id + 0x800,
                _ => a.application_id,
            };
            let metadata = read_snapshot_display(path, s.identity, options, false)?;
            let (mut name, display) = metadata.nacp.map(|n| nacp_texts(&n)).unwrap_or_default();
            if name.is_empty() {
                name = field(&metadata.title.unwrap_or_default());
            }
            println!(
                "@SNAPSHOT\t{}\t{kind}\t{:016x}\t{title:016x}\t{}\t{display}\t{name}",
                hex(&s.identity),
                a.application_id,
                s.numeric_version
            );
        }
    }
    for a in &summary.applications {
        for file in &a.source_files {
            println!(
                "@RESOURCE\t{:016x}\t{}\t{}\t{}",
                a.application_id,
                file.kind,
                file.size,
                field(&file.path)
            );
        }
        for bytes in &a.recipes {
            let info = recipe::recipe_info(bytes)?;
            println!(
                "@RECIPE\t{}\t{:016x}\t{}\t{}\t{}",
                hex(&info.recipe_sha256),
                a.application_id,
                info.size,
                info.recipe_bytes,
                field(&info.name)
            );
        }
    }
    println!("@RECLAIMABLE\t{}", reclaimable);
    if let Some(allocated) = allocated {
        println!("@ALLOCATED\t{allocated}");
    }
    println!("@SIZE\t{}", fs::metadata(path)?.len());
    Ok(())
}
/// Build each source's recipe (or read an `.nxrecipe`) and keep them all
/// inside `archive` in one edit. A source too large to keep is reported and
/// skipped: the NX stays complete for play.
fn keep_recipes(
    archive: &Path,
    sources: &[PathBuf],
    options: &WriterOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut recipes = Vec::new();
    let mut total = 0u64;
    for source in sources {
        let bytes = if source
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("nxrecipe"))
        {
            fs::read(source)?
        } else {
            let (bytes, s) = recipe::build_recipe(source, archive, options)?;
            print_recipe(source, &s);
            bytes
        };
        if total + bytes.len() as u64 > crate::MAX_EMBEDDED_RECIPE_BYTES / 2 {
            return Err(
                "rebuild data too large: exact source reconstruction cannot be guaranteed".into(),
            );
        }
        total += bytes.len() as u64;
        recipes.push(bytes);
    }
    if !recipes.is_empty() {
        recipe::embed_recipes(archive, recipes, options)?;
    }
    Ok(())
}
/// Recipe scans started with a conversion: each reads its source on its own
/// thread while the conversion reads it too, so the recipe needs no second
/// pass afterwards. Dropped without `keep` (the conversion failed), they stop.
struct RecipeScans {
    token: CancellationToken,
    jobs: Vec<RecipeJob>,
}
type RecipeJob = (
    PathBuf,
    Option<std::thread::JoinHandle<Result<recipe::SourceScan, crate::WriterError>>>,
);
impl RecipeScans {
    fn start(sources: &[PathBuf], on: bool, options: &WriterOptions) -> Self {
        let token = CancellationToken::default();
        let mut scan_options = options.clone();
        scan_options.cancellation = token.clone();
        let jobs = if on {
            sources
                .iter()
                .map(|source| {
                    let (path, options) = (source.clone(), scan_options.clone());
                    let job = std::thread::spawn(move || recipe::scan_source(&path, &options));
                    (source.clone(), Some(job))
                })
                .collect()
        } else {
            Vec::new()
        };
        Self { token, jobs }
    }
    /// Wait for every scan. A failed scan fails the command before it writes.
    fn join(&mut self) -> Result<Vec<(PathBuf, recipe::SourceScan)>, Box<dyn std::error::Error>> {
        let mut scans = Vec::with_capacity(self.jobs.len());
        for (source, job) in &mut self.jobs {
            let scan = job
                .take()
                .ok_or("missing recipe scan")?
                .join()
                .map_err(|_| "recipe scan panicked")??;
            scans.push((source.clone(), scan));
        }
        Ok(scans)
    }
    /// Requested reversibility is a contract: never warn and drop a recipe.
    fn keep(
        mut self,
        archive: &Path,
        options: &WriterOptions,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if self.jobs.is_empty() {
            return Ok(());
        }
        let scans = self.join()?;
        let known = recipe::archive_digests(archive, options)?;
        let mut recipes = Vec::new();
        for (source, bytes, stats) in finish_recipes(&scans, &known, options)? {
            print_recipe(&source, &stats);
            recipes.push(bytes);
        }
        recipe::embed_recipes(archive, recipes, options)?;
        Ok(())
    }
}
/// One recipe per scan against the archive's file set; all of them or an error.
fn finish_recipes(
    scans: &[(PathBuf, recipe::SourceScan)],
    known: &std::collections::HashSet<(u64, [u8; 32])>,
    options: &WriterOptions,
) -> Result<Vec<(PathBuf, Vec<u8>, recipe::RecipeStats)>, crate::WriterError> {
    let mut made = Vec::with_capacity(scans.len());
    let mut total = 0u64;
    for (source, scan) in scans {
        let (bytes, stats) = recipe::finish_recipe(scan, known, options)?;
        total += bytes.len() as u64;
        if total > crate::MAX_EMBEDDED_RECIPE_BYTES / 2 {
            return Err(crate::WriterError::new(
                crate::WriterErrorKind::ResourceLimit,
                "rebuild data too large: exact source reconstruction cannot be guaranteed",
            ));
        }
        made.push((source.clone(), bytes, stats));
    }
    Ok(made)
}
impl Drop for RecipeScans {
    fn drop(&mut self) {
        self.token.cancel();
        for (_, job) in &mut self.jobs {
            if let Some(job) = job.take() {
                let _ = job.join();
            }
        }
    }
}
fn parse_identity(text: &str) -> Result<[u8; 32], Box<dyn std::error::Error>> {
    if text.len() != 64 {
        return Err(format!("not a snapshot identity: {text}").into());
    }
    let mut id = [0u8; 32];
    for (i, b) in id.iter_mut().enumerate() {
        *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)?;
    }
    Ok(id)
}
/// `--frontend`: prints `@TOTAL` and `@PROGRESS` lines every 500 ms. With a
/// counter, the writer declares the file bytes it will read before reading
/// them (a NUP adds its decode first), `@PROGRESS` is the bytes read so far
/// and each growth of the total is printed as a new `@TOTAL` line (frontends
/// add them).
/// Without one, it is the size written: this process's temporary files
/// beside `target`, or the growth of `target` itself for in-place edits.
struct ProgressWatch {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
enum Watched {
    File { target: PathBuf, offset: u64 },
    Counter(ProgressCounter),
}
impl ProgressWatch {
    fn start(enabled: bool, target: &Path) -> Self {
        Self::start_offset(enabled, target, 0)
    }
    /// `offset` bytes were already done by earlier steps of the same command.
    fn start_offset(enabled: bool, target: &Path, offset: u64) -> Self {
        Self::spawn(
            enabled,
            Watched::File {
                target: target.to_path_buf(),
                offset,
            },
        )
    }
    fn counter(enabled: bool, counter: &ProgressCounter) -> Self {
        Self::spawn(enabled, Watched::Counter(counter.clone()))
    }
    fn spawn(enabled: bool, watched: Watched) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        if !enabled {
            return Self { stop, thread: None };
        }
        let flag = stop.clone();
        let sink = current_sink();
        let thread = std::thread::spawn(move || {
            SINK.with(|s| *s.borrow_mut() = sink);
            let mut printed_total = 0u64;
            let (dir, initial, marker) = match &watched {
                Watched::File { target, .. } => (
                    target
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."))
                        .to_path_buf(),
                    fs::metadata(target).map(|m| m.len()).unwrap_or(0),
                    format!("-{}-", std::process::id()),
                ),
                Watched::Counter(_) => (PathBuf::new(), 0, String::new()),
            };
            loop {
                let last = flag.load(std::sync::atomic::Ordering::Acquire);
                let done = match &watched {
                    Watched::Counter(c) => {
                        let total = c.total();
                        if total > printed_total {
                            println!("@TOTAL\t{}", total - printed_total);
                            printed_total = total;
                        }
                        c.done()
                    }
                    Watched::File { target, offset } => {
                        let mut written = fs::metadata(target)
                            .map(|m| m.len().saturating_sub(initial))
                            .unwrap_or(0);
                        if let Ok(entries) = fs::read_dir(&dir) {
                            for e in entries.flatten() {
                                let name = e.file_name().to_string_lossy().to_string();
                                if name.contains(&marker) && name.ends_with(".tmp") {
                                    written =
                                        written.max(e.metadata().map(|m| m.len()).unwrap_or(0));
                                }
                            }
                        }
                        offset + written
                    }
                };
                println!("@PROGRESS\t{done}");
                let _ = std::io::stdout().flush();
                if last {
                    break;
                }
                std::thread::park_timeout(std::time::Duration::from_millis(500));
            }
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for ProgressWatch {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
/// The caller's token in process; else with `--frontend`, a token that
/// cancels when stdin closes; else a fresh one.
fn command_token(o: &Options) -> CancellationToken {
    if let Some(token) = caller_token() {
        return token;
    }
    let token = CancellationToken::default();
    if o.frontend {
        // The frontend cancels by closing our stdin; the writer then removes
        // its temporary file and leaves the archive as it was.
        let watched = token.clone();
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
            watched.cancel();
        });
    }
    token
}
/// Removes the given files when dropped (`--no-trail` review files).
struct RemoveOnDrop(Vec<PathBuf>);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}
/// Decrypted DNSP/DXCI conversion and byte-exact rebuild recipes.
fn native_commands(args: &[String], o: &Options) -> Result<(), Box<dyn std::error::Error>> {
    if o.latest || !o.dlc.is_empty() || o.platform.is_some() || o.skip_dlc {
        return Err(usage().into());
    }
    if o.recipe.is_some() && args[0] != "pack" && args[0] != "add" {
        return Err("--recipe and --no-recipe apply to pack and add only".into());
    }
    let mut limits = import::ImportLimits::default();
    let mut options = WriterOptions::default();
    size_budgets(None, o, &mut limits, &mut options)?;
    options.cancellation = command_token(o);
    options.preserve_firmware = !o.omit_firmware;
    let started = Instant::now();
    match (args[0].as_str(), args.len()) {
        ("pack", n) if n >= 3 => {
            let output = Path::new(&args[n - 1]);
            if output.exists() {
                return Err("output already exists".into());
            }
            let sources: Vec<PathBuf> = args[1..n - 1].iter().map(PathBuf::from).collect();
            let recipes = RecipeScans::start(&sources, o.recipe != Some(false), &options);
            let native = InventoryOptions {
                mode: InventoryMode::Strict,
                ..InventoryOptions::default()
            };
            let inv = inventory(&sources, &native)?;
            for issue in inv.all_issues() {
                let level = match issue.severity {
                    native::Severity::Info => "info",
                    native::Severity::Warning => "warning",
                    native::Severity::Error => "error",
                };
                match issue.source.and_then(|i| inv.sources.get(i)) {
                    Some(s) => eprintln!("{level}: {}: {}", s.path.display(), issue.message),
                    None => eprintln!("{level}: {}", issue.message),
                }
            }
            let parent = output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let (stage_file, mut stage) =
                crate::writer::unique_temp(parent, std::ffi::OsStr::new(".nx-pack"))?;
            drop(stage_file);
            let watch = ProgressWatch::counter(o.frontend, &options.progress);
            let stats =
                convert_inventory(&inv, &stage.path, &native, IntegrityPolicy::Fast, &options)?;
            recipes.keep(&stage.path, &options)?;
            if options.cancellation.is_cancelled() {
                return Err("conversion cancelled before publication".into());
            }
            // The recipes were appended as a new generation of the private
            // stage: publish it as the only one.
            crate::writer::seal_staged_generations(&stage.path, &options)?;
            crate::writer::atomic_replace(&stage.path, output)?;
            stage.disarm();
            drop(watch);
            println!(
                "PASS sources={} source_bytes={} nx_bytes={} conversion_s={:.3}",
                sources.len(),
                sources
                    .iter()
                    .map(|s| fs::metadata(s).map(|m| m.len()).unwrap_or(0))
                    .sum::<u64>(),
                fs::metadata(output)?.len(),
                stats.elapsed.as_secs_f64()
            );
        }
        ("recipe", n) if n >= 3 => {
            let archive = Path::new(&args[1]);
            let sources: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
            keep_recipes(archive, &sources, &options)?;
            println!("@SIZE\t{}", fs::metadata(archive)?.len());
        }
        ("rebuild", n) if n >= 3 => {
            let archive = Path::new(&args[1]);
            let folder = Path::new(&args[2]);
            if !folder.is_dir() {
                return Err("the rebuild folder does not exist".into());
            }
            let wanted: Vec<String> = args[3..].iter().map(|a| a.to_ascii_lowercase()).collect();
            let all = recipe::embedded_recipes(archive, &options)?;
            let chosen: Vec<_> = all
                .iter()
                .filter(|r| wanted.is_empty() || wanted.contains(&hex(&r.info.recipe_sha256)))
                .collect();
            if chosen.is_empty() || (!wanted.is_empty() && chosen.len() != wanted.len()) {
                return Err("no such rebuild recipe in the archive".into());
            }
            for r in &chosen {
                let name = rebuild_name(&r.info.name).ok_or("the recipe has no safe file name")?;
                if folder.join(&name).exists() {
                    return Err(format!("{} already exists", folder.join(&name).display()).into());
                }
            }
            if o.frontend {
                println!(
                    "@TOTAL\t{}",
                    chosen.iter().map(|r| r.info.size).sum::<u64>()
                );
            }
            let mut done = 0u64;
            for r in chosen {
                let output = folder.join(rebuild_name(&r.info.name).unwrap());
                let watch = ProgressWatch::start_offset(o.frontend, &output, done);
                let s = recipe::rebuild_embedded(archive, &r.bytes, &output, &options)?;
                drop(watch);
                done += s.bytes;
                println!(
                    "@REBUILT\t{}\t{}\t{}",
                    field(&output.display().to_string()),
                    s.bytes,
                    hex(&s.sha256)
                );
            }
        }
        ("scan", n) if n >= 2 => {
            let sources: Vec<PathBuf> = args[1..].iter().map(PathBuf::from).collect();
            let native = InventoryOptions {
                mode: InventoryMode::Fast,
                ..InventoryOptions::default()
            };
            print_scan(&inventory(&sources, &native)?);
        }
        ("contents", 2) => print_contents(Path::new(&args[1]), &options)?,
        ("add", n) if n >= 3 => {
            let archive = Path::new(&args[1]);
            let sources: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
            let mut recipes = RecipeScans::start(&sources, o.recipe != Some(false), &options);
            let native = InventoryOptions {
                mode: InventoryMode::Strict,
                ..InventoryOptions::default()
            };
            let inv = inventory(&sources, &native)?;
            let present: Vec<(u64, u32, SnapshotType)> =
                inspect_archive(archive, &options, IntegrityPolicy::Fast)?
                    .applications
                    .iter()
                    .flat_map(|a| {
                        a.snapshots.iter().map(|s| {
                            let title = match s.snapshot_type {
                                SnapshotType::Dlc => dlc_title(archive, s, &options),
                                _ => a.application_id,
                            };
                            (title, s.numeric_version, s.snapshot_type)
                        })
                    })
                    .collect();
            let mut added = 0;
            // One watch for every addition: a new watch would print the
            // running total again as a new @TOTAL line.
            let _watch = ProgressWatch::counter(o.frontend, &options.progress);
            let mut listed = std::collections::HashSet::new();
            // (request, (kind, title, version), (application, version needed) of
            // a DLC the archive cannot run yet)
            type Pending<'a> = (NativeAddRequest, (&'a str, u64, u32), Option<(u64, u32)>);
            let mut pending: Vec<Pending> = Vec::new();
            for s in &inv.sources {
                for m in &s.metas {
                    let kind = meta_kind(m.meta.meta_type);
                    if kind == Some("base") {
                        return Err(format!(
                            "{} holds a base game; an NX holds one base, convert it instead",
                            s.path.display()
                        )
                        .into());
                    }
                    if kind.is_none() {
                        continue;
                    }
                    let owner = if kind == Some("dlc") {
                        m.meta.title_id
                    } else {
                        m.meta.application_id
                    };
                    // Same kind too: a base may carry its update's version
                    // number (Castlevania Belmont's Curse: both are 65536).
                    let snapshot_type = if kind == Some("dlc") {
                        SnapshotType::Dlc
                    } else {
                        SnapshotType::Update
                    };
                    if present
                        .iter()
                        .any(|&(t, v, ty)| t == owner && v == m.meta.version && ty == snapshot_type)
                    {
                        println!(
                            "@SKIPPED\t{}\t{:016x}\t{}\talready in the archive",
                            kind.unwrap(),
                            m.meta.title_id,
                            m.meta.version
                        );
                        continue;
                    }
                    let kind = kind.unwrap();
                    if !listed.insert((owner, m.meta.version, kind)) {
                        println!(
                            "@SKIPPED\t{kind}\t{:016x}\t{}\tlisted twice",
                            m.meta.title_id, m.meta.version
                        );
                        continue;
                    }
                    // Say which file and which game, instead of a base lookup
                    // error later (a mislabelled file holds another region's
                    // update, for example).
                    if !present
                        .iter()
                        .any(|&(t, _, ty)| t == m.meta.application_id && ty == SnapshotType::Base)
                    {
                        return Err(format!(
                            "{}: this {kind} is for game {:016x}, which is not in this archive; nothing was added",
                            s.path.display(),
                            m.meta.application_id
                        )
                        .into());
                    }
                    let waits = m
                        .meta
                        .required_application_version
                        .filter(|&need| {
                            kind == "dlc"
                                && !present.iter().any(|&(t, v, ty)| {
                                    t == m.meta.application_id
                                        && v >= need
                                        && ty != SnapshotType::Dlc
                                })
                        })
                        .map(|need| (m.meta.application_id, need));
                    let request = NativeAddRequest::new(&s.path, m.meta.title_id, m.meta.version);
                    pending.push((request, (kind, owner, m.meta.version), waits));
                }
            }
            // A DLC that needs a game version only an update of this same
            // command brings goes in a second write, after that update.
            // Everything else, failing members included, is in the first one.
            let mut writes: [Vec<Pending>; 2] = Default::default();
            let updates: Vec<(u64, u32)> = pending
                .iter()
                .filter(|(_, (kind, _, _), _)| *kind == "update")
                .map(|(_, (_, app, version), _)| (*app, *version))
                .collect();
            for item in pending {
                let later = item
                    .2
                    .is_some_and(|(app, need)| updates.iter().any(|&(a, v)| a == app && v >= need));
                if let (Some((app, need)), false) = (item.2, later) {
                    return Err(format!(
                        "{}: this DLC needs version {need} of game {app:016x}; neither the archive nor the given files have it; nothing was added",
                        item.0.native_source.display()
                    )
                    .into());
                }
                writes[usize::from(later)].push(item);
            }
            // Recipes are made inside the last write, from its staged files,
            // so that write publishes its content and its recipes, or neither.
            let scans = recipes.join()?;
            let owners: std::collections::HashMap<&Path, u64> = inv
                .sources
                .iter()
                .filter_map(|s| {
                    s.metas
                        .iter()
                        .find(|m| meta_kind(m.meta.meta_type).is_some())
                        .map(|m| (s.path.as_path(), m.meta.application_id))
                })
                .collect();
            let made = std::cell::RefCell::new(Vec::new());
            let make = |known: &std::collections::HashSet<(u64, [u8; 32])>| {
                let finished = finish_recipes(&scans, known, &options)?;
                let mut out = Vec::with_capacity(finished.len());
                for (source, bytes, _) in &finished {
                    let owner = owners.get(source.as_path()).ok_or_else(|| {
                        crate::WriterError::new(
                            crate::WriterErrorKind::InvalidInput,
                            "a rebuild recipe source names no game",
                        )
                    })?;
                    out.push((Platform::Switch1, *owner, bytes.clone()));
                }
                *made.borrow_mut() = finished;
                Ok(out)
            };
            let last = writes.iter().rposition(|w| !w.is_empty());
            // Each write adds its whole group in one new archive generation,
            // or nothing if any member fails.
            for (index, write) in writes.iter().enumerate().filter(|(_, w)| !w.is_empty()) {
                let requests: Vec<NativeAddRequest> =
                    write.iter().map(|(r, _, _)| r.clone()).collect();
                let deferred: Option<crate::writer::DeferredRecipes> =
                    (Some(index) == last && !scans.is_empty()).then_some(&make);
                crate::writer::add_native_contents_with_recipes(
                    archive,
                    archive,
                    &requests,
                    &native,
                    &options,
                    IntegrityPolicy::Fast,
                    deferred,
                )?;
                for (request, (kind, _, version), _) in write {
                    let title = request.title_id;
                    println!("@ADDED\t{kind}\t{title:016x}\t{version}");
                    added += 1;
                }
            }
            for (source, _, stats) in made.borrow().iter() {
                print_recipe(source, stats);
            }
            if added == 0
                && !inv.sources.iter().any(|s| {
                    s.metas
                        .iter()
                        .any(|m| meta_kind(m.meta.meta_type).is_some())
                })
            {
                return Err("no update or DLC found in the given files".into());
            }
            println!("@SIZE\t{}", fs::metadata(archive)?.len());
        }
        ("remove", n) if n >= 3 => {
            let archive = Path::new(&args[1]);
            let mut plan = RepackPlan::new();
            for id in &args[2..] {
                plan.remove_snapshots.push(parse_identity(id)?);
            }
            let watch = ProgressWatch::start(o.frontend, archive);
            let stats = repack_archive(archive, archive, plan, &options, IntegrityPolicy::Fast)?;
            drop(watch);
            println!("@REMOVED\t{}", stats.removed_snapshots);
            println!("@RECLAIMED\t{}", stats.reclaimed_disk_bytes);
            println!("@SIZE\t{}", fs::metadata(archive)?.len());
        }
        ("set-metadata", n) if n >= 4 => {
            let archive = Path::new(&args[1]);
            let id = args[2].trim_start_matches("0x");
            let mut edit = MetadataEdit {
                application_id: u64::from_str_radix(id, 16)
                    .map_err(|_| "APPLICATION_ID must be 16 hex digits")?,
                ..MetadataEdit::default()
            };
            for field in &args[3..] {
                match field.split_once('=') {
                    Some(("title", v)) => edit.title = Some(v.to_owned()),
                    Some(("publisher", v)) => edit.publisher = Some(v.to_owned()),
                    Some(("icon", v)) => edit.icon = Some(fs::read(v)?),
                    _ => return Err(format!("unknown metadata field: {field}").into()),
                }
            }
            let watch = ProgressWatch::start(o.frontend, archive);
            edit_archive_metadata(archive, vec![edit], &options, IntegrityPolicy::Fast)?;
            drop(watch);
            println!("@SIZE\t{}", fs::metadata(archive)?.len());
        }
        ("compact", 2) => {
            let archive = Path::new(&args[1]);
            if o.frontend {
                println!("@TOTAL\t{}", fs::metadata(archive)?.len());
            }
            let watch = ProgressWatch::start(o.frontend, archive);
            let policy = if o.no_compress {
                IntegrityPolicy::Strict
            } else {
                IntegrityPolicy::Fast
            };
            compact_archive(archive, &options, policy)?;
            drop(watch);
            println!("@SIZE\t{}", fs::metadata(archive)?.len());
        }
        ("check-recipe", 2) => {
            let archive = Path::new(&args[1]);
            let all = recipe::embedded_recipes(archive, &options)?;
            if all.is_empty() {
                return Err("the archive keeps no rebuild recipe".into());
            }
            for r in &all {
                let s = recipe::check_embedded(archive, &r.bytes, &options)?;
                println!(
                    "PASS {} rebuilds {} bytes sha256={}",
                    r.info.name,
                    s.bytes,
                    hex(&s.sha256)
                );
            }
        }
        _ => return Err(usage().into()),
    }
    println!("elapsed_s={:.6}", started.elapsed().as_secs_f64());
    Ok(())
}
fn run_command(argv: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    if argv.len() == 1 && matches!(argv[0].as_str(), "--version" | "-V") {
        println!("nxconvert {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (mut args, o) = parse(argv)?;
    if o.omit_firmware {
        if !matches!(args.first().map(String::as_str), Some("pack" | "add")) {
            return Err("--omit-firmware applies to native pack/add only".into());
        }
        if o.recipe != Some(false) {
            return Err(
                "--omit-firmware requires --no-recipe to skip exact source reconstruction".into(),
            );
        }
    }
    let mut limits = import::ImportLimits::default();
    let mut options = WriterOptions::default();
    let mut measured = None;
    if args.first().map(String::as_str) == Some("pack-nup") {
        return pack_nup(&args, &o);
    }
    if args.first().map(String::as_str) == Some("check-nup") {
        return check_nup(&args, &o);
    }
    if matches!(
        args.first().map(String::as_str),
        Some(
            "pack"
                | "recipe"
                | "rebuild"
                | "check-recipe"
                | "scan"
                | "contents"
                | "add"
                | "remove"
                | "compact"
                | "set-metadata"
        )
    ) {
        return native_commands(&args, &o);
    }
    if o.recipe.is_some() {
        return Err("--recipe and --no-recipe apply to pack and add only".into());
    }
    if o.skip_dlc {
        return Err("--skip-dlc applies to pack-nup only".into());
    }
    if args.first().map(String::as_str) == Some("add-tree") {
        return add_tree(&args, &o);
    }
    // --no-trail: review files of automatic pack-tree, removed once it ends.
    let mut _trail = RemoveOnDrop(Vec::new());
    if args.len() == 3 && args[0] == "pack-tree" {
        if o.latest || !o.dlc.is_empty() {
            return Err("--latest/--dlc require an explicit manifest".into());
        }
        let trail = if o.no_trail {
            format!("{}.trail-{}", args[2], std::process::id())
        } else {
            args[2].clone()
        };
        let plan = automatic_manifest(Path::new(&args[1]), &trail, &o)?;
        if o.no_trail {
            _trail.0 = [
                plan.manifest.clone(),
                format!("{trail}.assumptions.txt"),
                plan.receipt.clone(),
            ]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        }
        args.insert(2, plan.manifest);
        args.push(plan.receipt);
        measured = Some(plan.walk);
    } else if o.no_trail {
        return Err("--no-trail applies to pack-nup, automatic pack-tree and add-tree".into());
    } else if o.platform.is_some() {
        return Err("--platform applies to pack-nup and automatic pack-tree only".into());
    } else if (args.len() == 5 && args[0] == "pack-tree")
        || (args.len() == 4 && args[0] == "verify")
    {
        let package = Path::new(if args[0] == "verify" {
            &args[2]
        } else {
            &args[1]
        });
        if o.max_nodes.is_none() || o.inventory.is_none() || o.memory.is_none() {
            measured = Some(walk(package, NODE_CEILING)?);
        }
    }
    size_budgets(measured.as_ref(), &o, &mut limits, &mut options)?;
    if measured.is_some() {
        eprintln!(
            "BUDGETS max_nodes={} inventory_mib={} memory_mib={}",
            limits.max_nodes,
            limits.max_inventory_bytes / MIB,
            options.max_memory_bytes / MIB
        );
    }
    let started = Instant::now();
    match args.first().map(String::as_str) {
        Some("inspect-tree") if args.len() == 3 || args.len() == 4 => {
            if args.len() == 4 {
                let m = import::DeclarationManifest::read(File::open(&args[2])?, &limits)?;
                let view = import::inspect_tree(
                    Path::new(&args[1]),
                    &m,
                    &args[3],
                    &limits,
                    &options.cancellation,
                )?;
                println!(
                    "DECLARED {:?}\nOBSERVED {:?}\nCOMPLETENESS {:?}\nMODULES {:?}",
                    view.declaration(),
                    view.scope(),
                    view.completeness(),
                    view.modules()
                );
            } else {
                let profile = match args[2].as_str() {
                    "2048" => NpdmProfile::MetaAcid2048V1,
                    "3072" => NpdmProfile::MetaAcid3072V1,
                    "none" => NpdmProfile::Absent,
                    _ => return Err(usage().into()),
                };
                let audit = import::audit_executables(
                    Path::new(&args[1]),
                    profile,
                    &limits,
                    &options.cancellation,
                )?;
                println!("UNKNOWN application/platform/version/type/dependencies/completeness");
                println!(
                    "OBSERVED NPDM {:?}\nOBSERVED MODULES {:?}",
                    audit.npdm, audit.modules
                );
            }
        }
        Some("pack-tree") if args.len() == 5 => {
            if !o.latest && !o.dlc.is_empty() {
                return Err("--dlc requires --latest".into());
            }
            let mut m = import::DeclarationManifest::read(File::open(&args[2])?, &limits)?;
            if o.latest {
                let selection = m.select_latest_complete(&o.dlc, &limits)?;
                println!(
                    "OMITTED {:?}; DEPENDENCY CLOSURE {:?}",
                    selection.omitted_snapshot_keys, selection.retained_dependency_keys
                );
                m = selection.manifest;
            }
            let receipt_path = Path::new(&args[4]);
            let parent = receipt_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if parent
                .canonicalize()?
                .starts_with(Path::new(&args[1]).canonicalize()?)
            {
                return Err("receipt must be outside the read-only source package".into());
            }
            let receipt_real = parent
                .canonicalize()?
                .join(receipt_path.file_name().ok_or("receipt filename")?);
            let output = Path::new(&args[3]);
            let output_parent = output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let output_real = output_parent
                .canonicalize()?
                .join(output.file_name().ok_or("output filename")?);
            if receipt_real == output_real {
                return Err("archive and receipt paths must differ".into());
            }
            // Reserve a fresh receipt path first; never overwrite existing provenance.
            let mut receipt_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(receipt_path)?;
            options.cancellation = command_token(&o);
            let watch = ProgressWatch::counter(o.frontend, &options.progress);
            let converted =
                import::convert_extracted(Path::new(&args[1]), &m, output, &limits, &options);
            drop(watch);
            let result = match converted {
                Ok(r) => r,
                Err(e) => {
                    drop(receipt_file);
                    fs::remove_file(receipt_path)?;
                    return Err(e.into());
                }
            };
            eprintln!(
                "NX published. Receipt publication is separate; retain it alongside the archive."
            );
            receipt_file.write_all(&result.receipt.encode(&limits)?)?;
            receipt_file.sync_all()?;
            println!("PASS compared_bytes={} conversion_s={:.6} nx_bytes={} peak_temporary_bytes={} writer_accounted_bytes={} receipt={}", result.source_bytes_compared, result.elapsed.as_secs_f64(), fs::metadata(output)?.len(), result.peak_temporary_disk_bytes, result.initial.accounted_memory_bytes, receipt_path.display());
        }
        Some("info") if args.len() == 2 => {
            let summary = inspect_archive(&args[1], &options, IntegrityPolicy::Fast)?;
            for a in summary.applications {
                println!(
                    "APPLICATION {:?} {:016x}; DISPLAY SNAPSHOT {:?}",
                    a.platform,
                    a.application_id,
                    a.metadata_snapshot_identity.map(|id| hex(&id))
                );
                for p in a.programs {
                    println!("PROGRAM {:016x} {:?}", p.program_id, p.kind);
                }
                for s in a.snapshots {
                    println!("SNAPSHOT {} {:?} version={} program={:?} build={:?} content={:?} dependencies={:?}",hex(&s.identity),s.snapshot_type,s.numeric_version,s.program_id.map(|p|format!("{p:016x}")),s.build_id.map(|id|hex(&id)),s.content_id.map(|id|hex(&id)),s.dependencies);
                }
            }
        }
        Some("verify") if args.len() == 2 || args.len() == 4 => {
            if args.len() == 4 {
                let receipt = import::ImportReceipt::read(File::open(&args[3])?, &limits)?;
                let bytes = import::verify_extracted_sources(
                    Path::new(&args[1]),
                    Path::new(&args[2]),
                    &receipt,
                    &limits,
                    &options,
                )?;
                println!("PASS Strict and source byte comparison; compared_bytes={bytes}");
            } else {
                inspect_archive(&args[1], &options, IntegrityPolicy::Strict)?;
                println!("PASS Strict archive verification; no source comparison requested");
            }
        }
        Some("--help" | "help") => println!("{}", usage()),
        _ => return Err(usage().into()),
    }
    println!("elapsed_s={:.6}", started.elapsed().as_secs_f64());
    Ok(())
}

/// Output name of a rebuilt file. The recipe name comes from the archive, so
/// it is untrusted: only its last component is kept, which always lands
/// directly in the rebuild folder. On Windows a `:` would name an alternate
/// data stream, so such names are refused.
fn rebuild_name(name: &str) -> Option<std::ffi::OsString> {
    let file = Path::new(name).file_name()?;
    if file.is_empty() || (cfg!(windows) && file.to_string_lossy().contains(':')) {
        return None;
    }
    Some(file.to_owned())
}

#[cfg(test)]
mod tests {
    use super::rebuild_name;
    use std::path::{Component, Path};

    #[test]
    fn rebuild_names_stay_in_the_rebuild_folder() {
        let folder = Path::new("rebuild-folder");
        for name in [
            "game.nsp",
            "../../pwned",
            "sub/dir/pwned",
            "/abs/pwned",
            "pwned ",
            "pwned.",
            #[cfg(windows)]
            r"..\..\pwned",
            #[cfg(windows)]
            r"..\mixed/..\pwned",
            #[cfg(windows)]
            r"C:\Windows\pwned",
            #[cfg(windows)]
            "C:pwned",
            #[cfg(windows)]
            r"\\server\share\pwned",
            #[cfg(windows)]
            r"\\?\C:\pwned",
        ] {
            let file = rebuild_name(name).unwrap_or_else(|| panic!("{name:?} refused"));
            let joined = folder.join(&file);
            assert_eq!(joined.parent(), Some(folder), "{name:?} escapes");
            let parts: Vec<_> = Path::new(&file).components().collect();
            assert!(
                matches!(parts[..], [Component::Normal(_)]),
                "{name:?} -> {file:?}"
            );
        }
        for name in ["", ".", "..", "/", "a/..", "../"] {
            assert!(rebuild_name(name).is_none(), "{name:?} accepted");
        }
        #[cfg(windows)]
        assert!(rebuild_name("ab:stream").is_none());
    }
}
