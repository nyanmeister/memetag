pub mod autocomplete;
pub mod batch;
pub mod clipboard;
pub mod containers;
#[cfg(feature = "gui")]
pub mod dedup;
pub mod dupes;
#[cfg(feature = "gui")]
pub mod editor;
pub mod embed;
pub mod fuzz;
pub mod grab;
#[cfg(feature = "gui")]
pub mod grid;
#[cfg(feature = "gui")]
pub mod implications;
pub mod index;
pub mod library;
pub mod locking;
pub mod matroska;
pub mod meta;
pub mod ocr;
pub mod paths;
pub mod propose;
#[cfg(feature = "gui")]
pub mod propose_card;
pub mod pull;
pub mod query;
#[cfg(feature = "gui")]
pub mod search_complete;
pub mod serve;
pub mod setup;
pub mod similar;
pub mod sources;
pub mod tagger;
#[cfg(feature = "gui")]
pub mod tags_card;
pub mod verify;
pub mod viewer;
pub mod vocab;
#[cfg(feature = "gui")]
pub mod vocabconflict;
pub mod vocabsync;
#[cfg(feature = "gui")]
pub mod widgets;
pub mod writer;
pub mod xmp;
#[cfg(feature = "gui")]
pub mod xsel;
use crate::vocabsync::Canonical; // the `name()` used in the vocab command's messages is a trait method
use std::path::{Path, PathBuf};

const USAGE: &str = "memetag — tags live inside the files; the index is a cache.
  memetag --version | version              print the version, git revision and commit date
  memetag tag <file> [+tag|-tag|tag]...   add/remove tags in the file (merges with any existing XMP), then verifies
  memetag read <file>                     print the tags stored in the file
  memetag init <DIR>                      create initial configuration without replacing an existing one
  memetag doctor                          report paths and optional dependencies
  memetag sources [status|add NAME DIR|rename ID NAME|locate ID DIR|enable ID|disable ID]
  memetag sources add-network NAME MOUNTED_DIR [--server ID | --host HOST] [--server-root DIR]
                                          defaults to main's SSH settings and the new folder path;
                                          --pull-command CMD / --batch-command CMD override helpers
  memetag folders [--source ID] [status|include DIR|exclude DIR|all|none|menu]  choose indexed subfolders
  memetag reindex                         rebuild the SQLite index from enabled local sources
  memetag search <query>                  Boolean tag grammar: a, b || c, -d, (x || y), wild*, ns:value, width.gt:1000, format:gif, created_at.gt:2019, t.w:tea (whole word)
  memetag untagged                        files with no tags in their XMP (folder tags don't count)
  memetag pull [--dry-run] [--no-thumbs] [--start-ocr] [--force]  refresh the index from the server: new/changed files scanned there, gone rows dropped, OCR text kept across renames; refuses while the OCR service runs
  memetag tags                            tag list with counts
  memetag reimply                         apply vocab.toml (aliases, implications) to the index, no reindex
  memetag vocab status|sync|push|pull     share the alias/implication rules with the server; sync reconciles both ways,
                                          push/pull force a direction; on a conflict, --take-mine|--take-theirs|--merge
  memetag rescan <file>...                scan these files again into the index (after a format rule changed)
  memetag refresh-vocabulary              download a bounded supplemental tag vocabulary for offline autocomplete
  memetag grab [query]                    starts the optional memetag-gui browser\n  memetag grab [query]                    thumbnail window, newest files first, after a quick pull so just-saved files are in it (skipped while OCR runs)
                                          left-click copies and closes; middle-click copies and stays; Enter copies first hit and stays
                                          Edit on hover or Shift+left-click edits tags and OCR text; Ctrl+S saves (also applies a mass edit)
                                          right-click opens originals in feh (stills) or looping mpv (videos/GIF/APNG/animated WebP); copy errors leave the window open
                                          t: is an alias for text:, e.g. t:cat OR t:dog; videos play as storyboards in the grid
  memetag clip <file|id>                  put a file on the clipboard (image/png; GIFs as image/gif plus their file URL; others as text/uri-list)
  memetag thumbs                          (re)build the thumbnail cache for the index
  memetag ocr [--engine tesseract|ollama] [--model M] [--prompt P] [--limit N] [--all] [--embed] [--status]
                                          OCR every image the chosen engine has not done yet → `text` index (searchable as text:*words*);
                                          engine defaults to ollama when config.toml sets `ocr_model` (also `ocr_prompt`, `ollama_url`), else tesseract;
                                          one image per transaction, so stopping (SIGTERM/Ctrl-C) loses nothing and a rerun continues; --embed writes newly inferred text into files;
                                          use embed-text for already completed OCR results or to retry failed embedding without inference
  memetag meta [--limit N] [--dry-run] [--only TAG]
                                          act on meta: request tags and turn each into its done tag in the same write (it stays on failure):
                                          \"meta:translation request\" → English under \"English (machine):\" (translate_model/translate_prompt with {text}, else ocr_model);
                                          \"meta:reocr request\" → read the image again with ocr_model; \"meta:speech request\" → transcript of the audio track
                                          (speech_command with {wav}; files without audio keep the tag); \"meta:describe request\" → a few sentences about the picture
                                          to the OCR text under \"English (machine):\" and turns the tag into meta:translated in the same write; it stays on failure
  memetag embed-text [--db PATH] [--root DIR] [--source ID] [--limit N] [--dry-run]
                                          embed saved nonempty OCR text without inference; preserve mtime/inode, skip matching files, retry failures on rerun;
                                          reads the source index only; run bulk embedding on the file server using a snapshot of the OCR index
  memetag embeddings [--limit N]           image vectors (CLIP ViT-B/32, ONNX on the CPU) for every image not yet in the cache; the thumbnail is read,
                                          not the original; needs <data>/models/clip-vit-b32-vision.onnx or `embed_model` in config.toml
  memetag propose <tag> [--mode image|text|both] [--top N] [--measure [--seed N] [--pool tagged|all] [--rejects N]]
                                          files without the tag ranked by likeness to the files with it: image = cosine to the mean vector,
                                          text = TF-IDF over the OCR text, both = the sum; --measure hides a third of the tag's files and
                                          reports where they land among the hand-tagged files (--pool all: every file), per mode;
                                          --rejects N first rejects the N wrong offers a reviewer would have, top of the list down;
                                          the grab's Propose card offers the same list with checkboxes
  memetag similar <image> [--distance N] [--limit N] [--grab]
                                          library files that look like the image (a re-encode, resize, caption or small crop of it), closest first;
                                          <image> may be a file, a library path, or clipboard; N = max differing bits of 256, default 20;
                                          --grab shows them in the grid instead (same as grab similar:\"<image>\")
  memetag dupes [--distance N]            look-alike groups over the library (every member within N bits of every other), biggest first;
                                          hashes are cached in the index, filled on first use from the thumbnails; the grid has the same as its Duplicates card
  memetag export                          one JSON object per indexed file on stdout (tags by source, text) — the import feed for a booru
  memetag serve [--bind ADDR:PORT] [--share CMD]
                                          the grab grid as a web page (default http://127.0.0.1:7777/): search form, cached thumbnails newest first,
                                          tap a tile to run the share command on it (`termux-share` on a phone, `memetag clip` elsewhere;
                                          config.toml `share_command` with {file}, `share_stage` copies the pick under the cache first,
                                          `mount_command` runs once when an original is missing); ⤢ opens the original
  memetag recover                         replay the write journal (also runs at every start): restore files whose in-place write was interrupted
  memetag verify [--deep]                 audit: files present, XMP parses, ids/tags match the index, mtime unmoved; --deep re-hashes pixels
  index: config.toml `index_threads` (default = a quarter of the CPU threads) or env MEMETAG_THREADS
  video previews: config.toml `preview_fps` (default 4) and `strip_frames` (default 8), or env MEMETAG_FPS / MEMETAG_FRAMES for a quick trial
  grab window: config.toml `texture_budget_mb` (default 256; decoded tiles kept in GPU memory) or env MEMETAG_TEX_MB
  env MEMETAG_ROOT=<dir> overrides the root (default: ~/.config/memetag/config.toml `root`, else ~/.local/share/memetag/sample)";

#[derive(Clone)]
pub struct Cfg {
    pub sources: Option<Result<sources::Library, String>>,
    pub active_source: Option<String>,
    pub root: PathBuf,
    pub db: PathBuf,
    pub thumbs: PathBuf,
    pub vocab: vocab::Vocab,
    pub preview_fps: f64,
    pub strip_frames: u32,
    pub index_threads: usize,
    pub texture_budget_mb: usize,
    pub ocr_model: Option<String>,
    /// a text-only ollama model for `memetag meta` translations (gets the transcribed text, no image); None = ocr_model with the image
    pub translate_model: Option<String>,
    /// prompt for that model; `{text}` is replaced by the transcribed text
    pub translate_prompt: String,
    /// shell command that prints a transcript of `{wav}` (16 kHz mono) on stdout, for `meta:speech request`
    pub speech_command: String,
    pub ocr_prompt: String,
    pub ollama_url: String,
    /// ONNX file of the CLIP vision tower for tag proposals; None = <data>/models/clip-vit-b32-vision.onnx
    pub embed_model: Option<PathBuf>,
}

pub fn cfg() -> Cfg {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
    let cfg_dir = paths::config_dir();
    let data = paths::data_dir();
    let mut root = data.join("sample");
    let (mut fps, mut frames) = (4.0f64, 8u32);
    // whisper.cpp with large-v3 and Silero voice detection filters music and noise.
    // Install the executable and both model files separately; speech_command is configurable.
    let mut embed_model = None::<PathBuf>;
    let mut speech_command = String::from(
        "whisper-cli -m ~/.local/share/whisper/ggml-large-v3.bin --vad -vm ~/.local/share/whisper/ggml-silero-v5.1.2.bin -np -nt -f {wav}",
    );
    let (mut translate_model, mut translate_prompt) = (
        None::<String>,
        String::from("Translate the following segment into English, without additional explanation.\n\n{text}"),
    );
    let (mut ocr_model, mut ocr_prompt, mut ollama_url) = (
        None::<String>,
        String::from("Free OCR."),
        String::from("http://127.0.0.1:11434"),
    );
    let mut tex_mb = 256usize; // thumbnail texture budget; configurable with texture_budget_mb
    let mut threads = (std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        / 4)
    .max(1); // leave CPU capacity for foreground applications
    if let Ok(s) = std::fs::read_to_string(cfg_dir.join("config.toml")) {
        if let Ok(t) = s.parse::<toml::Table>() {
            if let Some(r) = t.get("root").and_then(|v| v.as_str()) {
                root = PathBuf::from(shellexpand(r, &home));
            }
            if let Some(v) = t
                .get("preview_fps")
                .and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64)))
            {
                fps = v;
            }
            if let Some(v) = t.get("strip_frames").and_then(|v| v.as_integer()) {
                frames = v.clamp(2, 64) as u32;
            }
            if let Some(v) = t.get("index_threads").and_then(|v| v.as_integer()) {
                threads = v.clamp(1, 64) as usize;
            }
            if let Some(v) = t.get("texture_budget_mb").and_then(|v| v.as_integer()) {
                tex_mb = v.clamp(64, 65536) as usize;
            }
            if let Some(v) = t.get("ocr_model").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    ocr_model = Some(v.to_string());
                }
            }
            if let Some(v) = t.get("ocr_prompt").and_then(|v| v.as_str()) {
                ocr_prompt = v.to_string();
            }
            if let Some(v) = t.get("translate_model").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    translate_model = Some(v.to_string());
                }
            }
            if let Some(v) = t.get("translate_prompt").and_then(|v| v.as_str()) {
                translate_prompt = v.to_string();
            }
            if let Some(v) = t.get("embed_model").and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    embed_model = Some(PathBuf::from(v));
                }
            }
            if let Some(v) = t.get("speech_command").and_then(|v| v.as_str()) {
                speech_command = v.to_string();
            }
            if let Some(v) = t.get("ollama_url").and_then(|v| v.as_str()) {
                ollama_url = v.trim_end_matches('/').to_string();
            }
        }
    }
    if let Some(v) = std::env::var("MEMETAG_TEX_MB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        tex_mb = v.clamp(64, 65536);
    }
    if let Some(v) = std::env::var("MEMETAG_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        threads = v.clamp(1, 64);
    }
    if let Ok(r) = std::env::var("MEMETAG_ROOT") {
        root = PathBuf::from(r);
    }
    if let Some(v) = std::env::var("MEMETAG_FPS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        fps = v;
    }
    if let Some(v) = std::env::var("MEMETAG_FRAMES")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
    {
        frames = v.clamp(2, 64);
    }
    let db = std::env::var_os("MEMETAG_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("index.sqlite"));
    let thumbs = std::env::var_os("MEMETAG_THUMBS")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::cache_dir().join("thumbs"));
    Cfg {
        sources: Some(sources::Library::load(&root).and_then(|library| {
            if std::env::var_os("MEMETAG_ROOT").is_some() && sources::path().exists() && library.source("main")?.path != root {
                return Err("MEMETAG_ROOT conflicts with the registered main source; use sources locate or a separate XDG_CONFIG_HOME for another library".into());
            }
            Ok(library)
        })),
        active_source: None,
        root,
        db,
        thumbs,
        vocab: vocab::Vocab::load(&cfg_dir.join("vocab.toml")),
        // the grid's tiles never play slower than 2 fps and clamp their rate to [2, preview_fps]: a ceiling under
        // 2 made that clamp panic in every decode worker (review, 2026-09-22)
        preview_fps: if fps.is_finite() {
            fps.clamp(2.0, 30.0)
        } else {
            4.0
        },
        strip_frames: frames,
        index_threads: threads,
        texture_budget_mb: tex_mb,
        ocr_model,
        translate_model,
        translate_prompt,
        speech_command,
        ocr_prompt,
        ollama_url,
        embed_model,
    }
}
/// This program's own path, read once at start: the helper processes (the clipboard owner, the OCR card's commands)
/// are spawned from it, and after a rebuild `current_exe()` ends in " (deleted)" for as long as a grid stays open.
pub fn self_exe() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| {
        let p = own_exe()?;
        if p.file_name().is_some_and(|n| n == "memetag-gui") {
            companion("memetag").ok()
        } else {
            Some(p)
        }
    })
    .clone()
}
fn shellexpand(s: &str, home: &Path) -> String {
    if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest).to_string_lossy().into_owned()
    } else {
        s.to_string()
    }
}

pub fn cli_main() {
    let a: Vec<String> = std::env::args().collect();
    // Side-effect-free, before recover/config, so it answers even with a broken index or share:
    if matches!(
        a.get(1).map(String::as_str),
        Some("--version") | Some("-V") | Some("version")
    ) {
        println!(
            "memetag {} ({}, {})",
            env!("CARGO_PKG_VERSION"),
            env!("MEMETAG_GIT"),
            env!("MEMETAG_COMMIT_DATE")
        );
        return;
    }
    if matches!(
        a.get(1).map(String::as_str),
        Some("--help") | Some("-h") | Some("help")
    ) {
        println!("{USAGE}");
        return;
    }
    let _ = self_exe();
    xmp::init(); // which XMP namespaces are ours, before any file is read
                 // an interrupted in-place write (power, network) leaves a journal entry; put the file back before doing anything else
    if !matches!(
        a.get(1).map(String::as_str),
        Some("_fuzz") | Some("_replay")
    ) {
        for line in writer::recover() {
            eprintln!("{line}");
        }
    }
    let code = match a.get(1).map(String::as_str) {
        Some("init") if a.len() == 3 => setup::init(&a[2]),
        Some("doctor") => setup::doctor(&cfg()),
        Some("tag") if a.len() >= 3 => cmd_tag(&a[2], &a[3..]),
        Some("read") if a.len() == 3 => cmd_read(&a[2]),
        Some("reindex") => cmd_reindex(),
        Some("folders") => library::run(&cfg(), &a[2..]),
        Some("sources") => sources::run(&cfg(), &a[2..]),
        Some("search") => cmd_search(&a[2..].join(" "), false),
        Some("serve") => serve::run(&cfg(), &a[2..]),
        Some("untagged") => cmd_search("tag_count:0", false),
        Some("tags") => cmd_tags(),
        Some("reimply") => cmd_reimply(),
        Some("vocab") => cmd_vocab(&a[2..]),
        Some("rescan") if a.len() >= 3 => cmd_rescan(&a[2..]),
        Some("refresh-vocabulary") => autocomplete::refresh(&cfg()),
        Some("_batch-worker") => batch::worker(),
        Some("_pull-worker") => pull::worker(),
        Some("_clip-owner") => {
            clipboard::serve(a.get(2).map(String::as_str).unwrap_or("CLIPBOARD"))
        }
        Some("pull") => pull::run(&cfg(), &a[2..]),
        Some("grab") => {
            let c = cfg();
            launch_gui(&c, &a[2..].join(" "))
        }
        Some("clip") if a.len() == 3 => grab::clip(&cfg(), &a[2]),
        Some("thumbs") => grab::build_thumbs(&cfg(), None),
        Some("ocr") => ocr::run(&cfg(), &a[2..]),
        Some("meta") => meta::run(&cfg(), &a[2..]),
        Some("embed-text") => embed::run(&cfg(), &a[2..]),
        Some("recover") => {
            let lines = writer::recover();
            if lines.is_empty() {
                println!("recover: journal empty, nothing to do");
            }
            Ok(())
        }
        Some("verify") => verify::run(&cfg(), a.iter().any(|x| x == "--deep")),
        Some("similar") => similar::run(&cfg(), &a[2..]),
        Some("embeddings") => propose::run_embeddings(&cfg(), &a[2..]),
        Some("propose") => propose::run(&cfg(), &a[2..]),
        Some("dupes") => {
            let d = a
                .iter()
                .position(|x| x == "--distance")
                .and_then(|i| a.get(i + 1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(similar::DEFAULT_DISTANCE);
            dupes::dupes(&cfg(), d)
        }
        Some("export") => dupes::export(&cfg()),
        Some("_replay") if a.len() == 3 => fuzz::replay(Path::new(&a[2])),
        Some("_fuzz") if a.len() >= 4 => fuzz::run(
            Path::new(&a[2]),
            a[3].parse().unwrap_or(30),
            a.get(4)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0x9E3779B97F4A7C15),
        ),
        _ => {
            eprintln!("{USAGE}");
            Err(String::new())
        }
    };
    if let Err(e) = code {
        if !e.is_empty() {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
}

fn cmd_read(file: &str) -> Result<(), String> {
    let b = std::fs::read(file).map_err(|e| e.to_string())?;
    match containers::get_xmp(&b)? {
        None => println!("(no xmp)"),
        Some(p) => {
            let r = xmp::read(&p)?;
            for t in r.tags {
                println!("{t}");
            }
            for (k, v) in r.fields {
                println!("meme:{k} = {v}");
            }
        }
    }
    Ok(())
}

fn cmd_tag(file: &str, ops: &[String]) -> Result<(), String> {
    let c = cfg();
    let path = Path::new(file);
    match tagger::apply(&c, path, ops, &[]) {
        Ok(o) => {
            println!("{}", o.report.line(path, true));
            if o.report.ok() {
                Ok(())
            } else {
                Err(String::new())
            }
        }
        Err(e) => Err(e),
    }
}

fn cmd_reindex() -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    let t0 = std::time::Instant::now();
    if let Some(library) = c.library()? {
        let mut scanned = 0;
        let mut failed = 0;
        for source in library
            .sources
            .iter()
            .filter(|s| s.enabled && !s.scope.include.is_empty())
        {
            if source.remote.is_some() || batch::requires_remote(&source.path)? {
                eprintln!("{}: server source; use pull", source.name);
                continue;
            }
            let source_cfg = c.for_source(&source.id)?;
            match index::Db::open_cfg(&source_cfg)?.reindex_cfg(&source_cfg) {
                Ok((ok, bad)) => {
                    println!("{}: indexed {ok} files, {bad} skipped", source.name);
                    scanned += 1;
                }
                Err(e) => {
                    eprintln!("{}: {e}", source.name);
                    failed += 1;
                }
            }
        }
        if failed > 0
            || scanned == 0
                && library
                    .sources
                    .iter()
                    .any(|s| s.enabled && !s.scope.include.is_empty())
        {
            return Err(
                "Some sources were unavailable; cached rows retained. Use pull for server sources"
                    .into(),
            );
        }
    } else {
        if batch::requires_remote(&c.root)? {
            return Err("Network root: use pull".into());
        }
        let (ok, bad) = db.reindex(&c.root, &c.vocab, c.index_threads)?;
        println!(
            "indexed {ok} files ({bad} skipped) in {:.2}s",
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Rebuild the derived `implied` rows from vocab.toml as the Implications card's Save does, for rules and aliases
/// edited by hand. Reads no file.
/// Scan the named files again with the current rules; a few files read through the mount is fine, a reindex is not.
fn cmd_rescan(files: &[String]) -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    for f in files {
        let p = Path::new(f);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            if c.library()?
                .is_some_and(|library| library.identify(f).is_ok())
            {
                c.file_path(f)?
            } else {
                let root = c
                    .library()?
                    .map(|library| library.source("main").map(|s| &s.path))
                    .transpose()?
                    .unwrap_or(&c.root);
                root.join(p)
            }
        };
        let rel = c.file_key(&full)?;
        if !db.scope.contains(Path::new(&rel)) {
            return Err("file is outside the selected folders".into());
        }
        let bytes = batch::read_for_edit(&c, &full)?;
        let row = db.upsert_cfg(&c, &full, &bytes)?;
        println!(
            "{}  {} {} {}×{}",
            row.path, row.format, row.kind, row.width, row.height
        );
    }
    Ok(())
}

fn cmd_reimply() -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    let changed = db.reimply(&c.vocab)?;
    println!("{} files changed", changed.len());
    Ok(())
}

/// After the local vocab.toml was replaced by a sync (pull, take-theirs, merge), rebuild the derived index rows so the
/// index matches the new rules. `cfg()` reloads the vocabulary from disk, so it already sees the change.
fn reimply_after() -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    let n = db.reimply(&c.vocab)?.len();
    println!("  implications reapplied: {n} files changed in the index");
    Ok(())
}

fn print_conflict(cf: &vocabsync::Conflict, store: &dyn vocabsync::Canonical) {
    eprintln!(
        "vocab: your rules and {}'s both changed since the last sync — nothing was written.",
        store.name()
    );
    for l in &cf.diff.only_local {
        eprintln!("  only here:   {l}");
    }
    for l in &cf.diff.only_canon {
        eprintln!("  only server: {l}");
    }
}

fn cmd_vocab(args: &[String]) -> Result<(), String> {
    let c = cfg();
    let env = vocabsync::Env::from_home();
    let Some(store) = vocabsync::store_for(&c)? else {
        return Err(
            "no [pull_remote] server is configured, so there is nothing to sync the vocabulary against".into(),
        );
    };
    let flag = |name: &str| args.iter().any(|a| a == name);
    match args.first().map(String::as_str).unwrap_or("status") {
        "status" => {
            let out = vocabsync::sync_dry(&store, &env)?;
            println!("{out}");
            Ok(())
        }
        "push" => {
            vocabsync::take_mine(&store, &env)?;
            println!("vocab: pushed your rules to {}", store.name());
            Ok(())
        }
        "pull" => {
            vocabsync::take_theirs(&store, &env)?;
            reimply_after()?;
            println!("vocab: took {}'s rules", store.name());
            Ok(())
        }
        "sync" => {
            if flag("--take-mine") {
                vocabsync::take_mine(&store, &env)?;
                println!("vocab: kept your rules and pushed them to {}", store.name());
                return Ok(());
            }
            if flag("--take-theirs") {
                vocabsync::take_theirs(&store, &env)?;
                reimply_after()?;
                println!("vocab: took {}'s rules", store.name());
                return Ok(());
            }
            if flag("--merge") {
                let (_, clashes) = vocabsync::preview_merge(&store, &env)?;
                if !clashes.is_empty() {
                    eprintln!("vocab: the merge is clean except for {} alias(es) both sides define differently:", clashes.len());
                    for cl in &clashes {
                        eprintln!(
                            "  {} = {} (here) vs {} (server)",
                            cl.key, cl.local, cl.canon
                        );
                    }
                    return Err("resolve those by editing the alias, then rerun, or choose --take-mine / --take-theirs".into());
                }
                vocabsync::resolve_merge(&store, &env, &[])?;
                reimply_after()?;
                println!(
                    "vocab: merged both rule sets and pushed to {}",
                    store.name()
                );
                return Ok(());
            }
            match vocabsync::sync(&store, &env)? {
                vocabsync::Outcome::InSync => {
                    println!("vocab: already in sync with {}", store.name())
                }
                vocabsync::Outcome::Seeded => {
                    println!("vocab: seeded {} from your rules", store.name())
                }
                vocabsync::Outcome::Pushed => {
                    println!("vocab: pushed your changes to {}", store.name())
                }
                vocabsync::Outcome::Pulled => {
                    println!("vocab: pulled {}'s changes", store.name());
                    reimply_after()?;
                }
                vocabsync::Outcome::Conflict(cf) => {
                    print_conflict(&cf, &store);
                    return Err(
                        "vocab: conflict — rerun with --take-mine, --take-theirs, or --merge"
                            .into(),
                    );
                }
            }
            Ok(())
        }
        other => Err(format!(
            "unknown vocab subcommand {other:?}; use status, sync, push or pull"
        )),
    }
}

fn cmd_tags() -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    for (t, n) in db.tag_counts()? {
        println!("{n:6}  {t}");
    }
    Ok(())
}

fn cmd_search(q: &str, grab: bool) -> Result<(), String> {
    let c = cfg();
    let db = index::Db::open_cfg(&c)?;
    let mut expr = query::parse(q)?;
    sources::resolve_query(&c, &mut expr)?;
    let rows = db.all()?;
    let alias = |t: &str| c.vocab.canon(t);
    // similar: terms need the hash cache; filled here (with progress) only when the query has one
    let wanted = query::similar_terms(&expr);
    let mut lookup = similar::Lookup::new(if wanted.is_empty() {
        Default::default()
    } else {
        similar::ensure(&c, &db, &rows, true)?
    });
    if let Some(e) = lookup.prepare(&wanted).into_iter().next() {
        return Err(e);
    }
    let similar = |v: &str, p: &str| lookup.distance(v, p);
    let hits: Vec<&index::FileRow> = rows
        .iter()
        .filter(|f| query::eval(&expr, &query::Item::of(f, &similar), &alias))
        .collect();
    if grab {
        return grab::grab(&c, &db, q, &hits);
    }
    for f in &hits {
        println!("{}", c.file_path(&f.path)?.display());
    }
    eprintln!("{} of {} files match {:?}", hits.len(), rows.len(), q);
    Ok(())
}

#[cfg(test)]
pub mod property_tests;

pub fn version(program: &str) {
    println!(
        "{program} {} ({}, {})",
        env!("CARGO_PKG_VERSION"),
        env!("MEMETAG_GIT"),
        env!("MEMETAG_COMMIT_DATE")
    );
}

/// Companions install together. Resolve beside the original executable before searching PATH.
fn own_exe() -> Option<PathBuf> {
    static EXE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).clone()
}

pub fn companion(name: &str) -> Result<PathBuf, String> {
    if let Some(p) = own_exe().and_then(|p| p.parent().map(|p| p.join(name))) {
        if p.is_file() {
            return Ok(p);
        }
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(format!(
        "{name} is not installed; install the corresponding optional memetag component"
    ))
}

pub fn launch_gui(_c: &Cfg, query: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    Err(std::process::Command::new(companion("memetag-gui")?)
        .arg(query)
        .exec()
        .to_string())
}

#[cfg(feature = "gui")]
pub fn gui_main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--version" | "-V"))
    {
        version("memetag-gui");
        return;
    }
    if args.iter().any(|a| matches!(a.as_str(), "--help" | "-h")) {
        println!("memetag-gui [query] | --folders | --version");
        return;
    }
    let _ = self_exe();
    xmp::init(); // which XMP namespaces are ours, before any file is read
    for line in writer::recover() {
        eprintln!("{line}");
    }
    let c = cfg();
    let result = if args.first().is_some_and(|a| a == "--folders") {
        library_ui::run(&c)
    } else {
        grid::run(&c, &args.join(" "))
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(feature = "gui")]
mod library_ui;
