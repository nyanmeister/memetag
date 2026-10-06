//! `memetag ocr` — text out of every image into the `text` FTS table (searchable as text:*words*).
//! Two engines: **tesseract** (local CLI, parallel) and **ollama** (a local vision/OCR model over HTTP, sequential — the
//! GPU is the bottleneck; a reader thread fetches and encodes the next file while the model works on this one).
//! `text_meta` records which engine wrote each row, so a rerun does only what that engine has not done.
//! Every image is its own transaction; SIGTERM/SIGINT finish the image in flight, print a summary and exit 0 —
//! built to run under a systemd unit that can be stopped and started again at will (2026-09-12).
use crate::index::Db;
use crate::Cfg;
use rusqlite::params;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::time::Instant;

struct Opts {
    engine: String,
    model: String,
    prompt: String,
    limit: usize,
    all: bool,
    embed: bool,
    status: bool,
}

fn parse(c: &Cfg, args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        engine: if c.ocr_model.is_some() {
            "ollama".into()
        } else {
            "tesseract".into()
        },
        model: c.ocr_model.clone().unwrap_or_default(),
        prompt: c.ocr_prompt.clone(),
        limit: 0,
        all: false,
        embed: false,
        status: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--engine" => {
                o.engine = args.get(i + 1).cloned().ok_or("--engine needs a value")?;
                i += 1;
            }
            "--model" => {
                o.model = args.get(i + 1).cloned().ok_or("--model needs a value")?;
                o.engine = "ollama".into();
                i += 1;
            }
            "--prompt" => {
                o.prompt = args.get(i + 1).cloned().ok_or("--prompt needs a value")?;
                i += 1;
            }
            "--limit" => {
                o.limit = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .ok_or("--limit needs a number")?;
                i += 1;
            }
            "--all" => o.all = true,
            "--embed" => o.embed = true,
            "--status" => o.status = true,
            x => return Err(format!("unknown option {x}")),
        }
        i += 1;
    }
    if o.engine == "ollama" && o.model.is_empty() {
        return Err("no model: set `ocr_model` in config.toml or pass --model".into());
    }
    if o.engine != "ollama" && o.engine != "tesseract" {
        return Err(format!("unknown engine {}", o.engine));
    }
    Ok(o)
}

/// The engine label stored in text_meta: "tesseract" or "ollama:<model>".
fn label(o: &Opts) -> String {
    if o.engine == "ollama" {
        format!("ollama:{}", o.model)
    } else {
        "tesseract".into()
    }
}
/// The label a plain `memetag ocr` would use with this config (the grid's "awaiting OCR" badge keys on it).
pub fn current_label(c: &Cfg) -> String {
    match &c.ocr_model {
        Some(m) => format!("ollama:{m}"),
        None => "tesseract".into(),
    }
}

/// One line of the grid's OCR card: prose, or a command with Run and Copy buttons beside it.
pub enum HelpLine {
    Text(String),
    Cmd(&'static str, Run),
}
/// How the card runs a command: `Quiet` ones finish in a moment and their output shows in the card;
/// `Terminal` ones stream or run for hours, so they open in a terminal window of their own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Run {
    Quiet,
    Terminal,
}

/// The argv for a card command: a leading `memetag` becomes this very binary, so a grid started from a launcher
/// without ~/.local/bin on its PATH still runs the matching build.
fn argv(cmd: &str) -> Vec<String> {
    let mut v: Vec<String> = cmd.split_whitespace().map(str::to_string).collect();
    if v.first().map(String::as_str) == Some("memetag") {
        if let Some(exe) = crate::self_exe() {
            v[0] = exe.to_string_lossy().into_owned();
        }
    }
    v
}

/// Run a `Quiet` card command to completion; the report is what the card shows under it: the command's output,
/// or its exit status when it printed nothing, and after a systemctl verb the unit's state, so start/stop have a visible outcome.
pub fn run_quiet(cmd: &str) -> String {
    let v = argv(cmd);
    let out = match Command::new(&v[0])
        .args(&v[1..])
        .stdin(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => return format!("{}: {e}", v[0]),
    };
    let mut text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim_end().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if text.is_empty() {
        text = if out.status.success() {
            "done".into()
        } else {
            format!("exited with {}", out.status)
        };
    }
    if v[0] == "systemctl" {
        text.push_str(&format!("\nmemetag-ocr.service is {}", service_state()));
    }
    text
}
/// `active`, `inactive`, `failed`… as systemctl reports it; `unknown` when systemctl itself is missing.
pub fn service_state() -> String {
    Command::new("systemctl")
        .args(["--user", "is-active", "memetag-ocr.service"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Open a `Terminal` card command in a terminal window of its own: $TERMINAL first, then the usual suspects; the window
/// holds after the command ends so a summary stays readable. Detached into its own session, so it outlives the grid.
/// Returns the terminal's name for the toast.
pub fn run_in_terminal(cmd: &str) -> Result<String, String> {
    let v = argv(cmd);
    let known: &[(&str, &[&str])] = &[
        ("kitty", &["--hold"]),
        ("alacritty", &["--hold", "-e"]),
        ("foot", &["--hold"]),
        ("xterm", &["-hold", "-e"]),
        ("x-terminal-emulator", &["-e"]),
    ];
    let on_path = |name: &str| {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
            .unwrap_or(false)
    };
    let (term, flags): (String, Vec<&str>) =
        match std::env::var("TERMINAL").ok().filter(|t| !t.is_empty()) {
            Some(t) => {
                let name = std::path::Path::new(&t)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (
                    t,
                    known
                        .iter()
                        .find(|(k, _)| *k == name)
                        .map(|(_, f)| f.to_vec())
                        .unwrap_or_else(|| vec!["-e"]),
                )
            }
            None => known
                .iter()
                .find(|(k, _)| on_path(k))
                .map(|(k, f)| (k.to_string(), f.to_vec()))
                .ok_or(
                    "no terminal found: set $TERMINAL, or install kitty, alacritty, foot or xterm",
                )?,
        };
    let mut c = Command::new(&term);
    c.args(&flags)
        .args(&v)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = c.spawn().map_err(|e| format!("{term}: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(std::path::Path::new(&term)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(term))
}

/// The OCR card behind the grid's "OCR" button.
///
/// Portability, for whoever adapts this: the engine, model, prompt and server come from config.toml and are read here
/// at runtime, so a newer model needs no edit — only the `ocr_model` line. The **systemd** section describes the units
/// shipped in `systemd/` (user units, `ollama.service` as a dependency) and is Linux-with-systemd specific; on another
/// init system, or on macOS, drop that section and keep "By hand": `memetag ocr` is the whole pass, resumable on its own.
/// A third engine means a new `label` above, a new arm in `run`, and a sentence here.
pub fn help(c: &Cfg, awaiting: usize) -> Vec<(&'static str, Vec<HelpLine>)> {
    use HelpLine::*;
    let engine = current_label(c);
    let which = match &c.ocr_model {
        Some(m) => format!("Engine now: ollama model `{m}` at {} (config.toml: ocr_model, ollama_url, ocr_prompt). Without an ocr_model line the tesseract CLI is used instead.", c.ollama_url),
        None => "Engine now: tesseract (the CLI must be installed). Set ocr_model in config.toml to use a local ollama vision model instead; ollama_url and ocr_prompt go with it.".into(),
    };
    vec![
        ("What it is", vec![
            Text("A pass over every image reads the text in it into the index, so t:word (or text:word, t.w:word for whole words) finds memes by what they say. Tiles marked OCR have not been read by the current engine yet; hover a tile to see its text.".into()),
            Text(format!("Engine label in the index: {engine}. {awaiting} of the images in this search still await it.")),
            Text(which),
        ]),
        ("With systemd (the units in the repo's systemd/ folder, installed under ~/.config/systemd/user/)", vec![
            Text("Start the pass; it runs at low priority beside the desktop and exits when every image is done. Stop it any time: the image in flight is finished, nothing is lost, and the next start continues where it left off.".into()),
            Cmd("systemctl --user start memetag-ocr.service", Run::Quiet),
            Cmd("systemctl --user stop memetag-ocr.service", Run::Quiet),
            Cmd("journalctl --user -fu memetag-ocr.service", Run::Terminal),
            Text("The optional memetag-pull.timer periodically merges new files from configured sources and starts this unit when unread images arrive. Choose its schedule before enabling it.".into()),
        ]),
        ("By hand (no systemd, or to watch it)", vec![
            Text("The same pass in a terminal (Run opens one); Ctrl-C stops after the current image and a rerun continues. --limit N does N images, --status prints progress per engine, --engine/--model/--prompt override config.toml for one run.".into()),
            Cmd("memetag ocr", Run::Terminal),
            Cmd("memetag ocr --status", Run::Quiet),
            Cmd("memetag ocr --limit 50", Run::Terminal),
        ]),
        ("Action tags (meta:)", vec![
            Text(crate::meta::ACTIONS.iter().map(|a| format!("\"{}\" → \"{}\": {}.", a.request, a.done, a.what)).collect::<Vec<_>>().join(" ")),
            Text("Tag a file with one in the editor or a mass edit, then run the pass here; it reads the image with the ollama model above, translates with config.toml translate_model when set (a text-only translator such as Hunyuan-MT; translate_prompt with {text}) and writes the file directly, so the request tag changes only when the work landed. Reviewed text keeps its mark. Request tags stay in the autocomplete even when no file carries them.".into()),
            Cmd("memetag meta", Run::Terminal),
            Cmd("memetag meta --dry-run", Run::Quiet),
            Text("Speech needs a transcriber on the PATH: config.toml speech_command, by default whisper.cpp's whisper-cli (CPU or an accelerated build) with ~/.local/share/whisper/ggml-large-v3.bin and the Silero VAD model beside it; ffprobe checks for an audio track first, and voice detection turns music into no text rather than a guess.".into()),
        ]),
        ("Good to know", vec![
            Text("New files reach the index through memetag pull (grab runs one before opening); OCR only sees indexed images. While the pass writes the index, pull and grab's pull step stand aside.".into()),
            Text("Changing ocr_model starts over: the pass redoes every image with the new model, keeping old text until each is replaced. Text you typed in the editor is never overwritten by any engine.".into()),
            Text("OCR text lives in the index until embedded in the files. memetag ocr --embed writes newly read text; memetag embed-text writes existing results. Back up the collection before a bulk pass. For network sources, run bulk embedding on the file server using a consistent index backup, as described in docs/legacy-workflow.md. Until embedded, text does not travel with copied files or survive a fresh index. memetag ocr --status prints how many images await embedding.".into()),
        ]),
    ]
}

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let o = parse(c, args)?;
    let db = Db::open_cfg(&c)?;
    if o.status {
        return status(&db);
    }
    let want = label(&o);
    let rows = db.all()?;
    let reviewed: std::collections::HashSet<String> = {
        let mut st = db
            .conn
            .prepare("SELECT path FROM manual_text")
            .map_err(|e| e.to_string())?;
        let values = st
            .query_map([], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<_, _>>()
            .map_err(|e| e.to_string())?;
        values
    };
    // done = paths this engine already wrote (unless --all); text from another engine gets redone by this one
    let done: std::collections::HashSet<String> = if o.all {
        Default::default()
    } else {
        let mut st = db
            .conn
            .prepare("SELECT path FROM text_meta WHERE engine=?1")
            .map_err(|e| e.to_string())?;
        let v: std::collections::HashSet<String> = st
            .query_map(params![want], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        v
    };
    let mut todo: Vec<(String, String, String)> = rows
        .iter()
        .filter(|r| r.kind == "image" && !done.contains(&r.path) && !reviewed.contains(&r.path))
        .map(|r| (r.id.clone(), r.path.clone(), r.format.clone()))
        .collect();
    if o.limit > 0 {
        todo.truncate(o.limit);
    }
    let total = todo.len();
    let remaining_all = rows
        .iter()
        .filter(|r| r.kind == "image" && !done.contains(&r.path) && !reviewed.contains(&r.path))
        .count();
    if total == 0 {
        println!("ocr [{want}]: nothing to do — every image is done");
        return Ok(());
    }
    eprintln!(
        "ocr [{want}]: {total} images to do{} ({remaining_all} not yet done by this engine)",
        if o.limit > 0 { " (limited)" } else { "" }
    );
    // graceful stop: SIGTERM/SIGINT → finish the image in flight, then summarise and exit 0
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        ctrlc::set_handler(move || {
            s.store(true, Ordering::SeqCst);
            eprintln!("ocr: stop requested — finishing the current image");
        })
        .map_err(|e| e.to_string())?;
    }
    if o.engine == "ollama" {
        run_ollama(c, &db, &o, &want, todo, stop)
    } else {
        run_tesseract(c, &db, &o, &want, todo, stop)
    }
}

fn status(db: &Db) -> Result<(), String> {
    let images: i64 = db
        .conn
        .query_row("SELECT count(*) FROM files WHERE kind='image' AND path NOT IN (SELECT path FROM excluded_files)", [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    let with_text: i64 = db
        .conn
        .query_row("SELECT count(*) FROM text WHERE length(body)>0 AND path NOT IN (SELECT path FROM excluded_files)", [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    println!("images in the index: {images}\nimages with any text: {with_text}");
    let pending = embed_pending(db)?;
    if pending > 0 {
        println!(
            "machine OCR text in the index only, not yet in the files: {pending}\n  \
             (run memetag embed-text after backing up the collection; for network files, run it on the file server\n   \
             until then this text does not reach the server files, other machines, or a reindex there)"
        );
    } else {
        println!("machine OCR text in the index only, not yet in the files: 0 (all read text is embedded)");
    }
    // counted against the files table, so rows for images that have since left the collection do not skew "remaining"
    let mut st = db.conn.prepare("SELECT m.engine, count(*), round(avg(m.secs),2), max(m.at) FROM text_meta m JOIN files f ON f.path=m.path WHERE f.kind='image' AND f.path NOT IN (SELECT path FROM excluded_files) GROUP BY m.engine ORDER BY 2 DESC").map_err(|e| e.to_string())?;
    let v: Vec<(String, i64, Option<f64>, f64)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    for (e, n, secs, at) in v {
        println!(
            "  {e:24} {n:6} done · {:.2} s/img avg · last {}",
            secs.unwrap_or(0.0),
            crate::index::fmt_time(at)
        );
        println!("  {:24} {:6} remaining for this engine", "", images - n);
    }
    Ok(())
}
/// Images whose machine OCR text lives only in this index — read by a model but never written into the file's own XMP.
/// `text_in_file` is set from each scan (see index::write_row); a human-reviewed file is excluded, its text being in the
/// file by definition. This is the population a server-side `embed-text` pass embeds, and the count that should fall to
/// near zero after it runs and the desktop pulls the re-scanned files. Cheap: one indexed count over the files table.
pub fn embed_pending(db: &Db) -> Result<i64, String> {
    db.conn
        .query_row(
            "SELECT count(*) FROM files f JOIN text t ON t.path=f.path \
             WHERE f.kind='image' AND f.path NOT IN (SELECT path FROM excluded_files) AND f.text_in_file=0 AND length(t.body)>0 \
             AND f.path NOT IN (SELECT path FROM manual_text)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())
}

fn store(
    db: &Db,
    c: &Cfg,
    o: &Opts,
    want: &str,
    rel: &str,
    text: &str,
    secs: f64,
) -> Result<bool, String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM text WHERE path=?1", params![rel])
        .ok();
    tx.execute(
        "INSERT INTO text(path, body) VALUES(?1,?2)",
        params![rel, text],
    )
    .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT OR REPLACE INTO text_meta(path, engine, at, secs) VALUES(?1,?2,?3,?4)",
        params![rel, want, now, secs],
    )
    .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    let mut embedded = false;
    if o.embed && !text.is_empty() {
        match crate::tagger::set_field(c, &c.file_path(rel)?, "text", text) {
            Ok(()) => embedded = true,
            Err(e) => eprintln!("embed failed {rel}: {e}"),
        }
    }
    Ok(embedded)
}

/// Bytes to send: JPEG/PNG as they are; anything else (GIF, WebP, BMP, …) re-encoded as a PNG of the first frame.
pub fn image_bytes(path: &std::path::Path, format: &str) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(path).map_err(|e| e.to_string())?;
    if matches!(format, "jpeg" | "png") {
        return Ok(raw);
    }
    let img = crate::containers::decode(&raw).map_err(|e| format!("decode: {e}"))?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.to_rgba8()
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("png: {e}"))?;
    Ok(out.into_inner())
}

fn run_ollama(
    c: &Cfg,
    db: &Db,
    o: &Opts,
    want: &str,
    todo: Vec<(String, String, String)>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let url = format!("{}/api/generate", c.ollama_url);
    crate::ollama::reachable(&c.ollama_url)?;
    // Check the configured server's model once, rather than reporting a 404 for every image.
    // Clients without an OCR engine can still read text embedded by another machine.
    if !crate::ollama::has_model(&c.ollama_url, &o.model)? {
        return Err(format!(
            "the OCR model `{}` is not installed in ollama at {}. Install it with `ollama pull {}` \
             on that server, or configure ocr_model and ollama_url for an available OCR engine. \
             Reading text already embedded in files does not require an OCR model.",
            o.model, c.ollama_url, o.model
        ));
    }
    let total = todo.len();
    let reader_cfg = c.clone();
    // reader thread: one file ahead, so the mount latency overlaps the model's work
    let (tx, rx) = mpsc::sync_channel::<(String, String, Result<Vec<u8>, String>)>(2);
    let stop_r = stop.clone();
    let reader = std::thread::spawn(move || {
        for (_id, rel, format) in todo {
            if stop_r.load(Ordering::SeqCst) {
                break;
            }
            let b = reader_cfg
                .file_path(&rel)
                .and_then(|p| image_bytes(&p, &format));
            if tx.send((rel, format, b)).is_err() {
                break;
            }
        }
    });
    let (mut n, mut with_text, mut errs, mut embedded, mut consecutive_errs) =
        (0usize, 0usize, 0usize, 0usize, 0usize);
    let t0 = Instant::now();
    let agent = crate::ollama::agent();
    let mut rc = Ok(());
    for (rel, _format, bytes) in rx.iter() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let t = Instant::now();
        let res = bytes.and_then(|b| {
            crate::ollama::generate(&agent, &url, &o.model, &o.prompt, Some(&b), 700)
        });
        let secs = t.elapsed().as_secs_f64();
        n += 1;
        match res {
            Err(e) => {
                errs += 1;
                consecutive_errs += 1;
                eprintln!("skip {rel}: {e}");
                if consecutive_errs >= 20 {
                    rc = Err("20 consecutive failures — is the share mounted and ollama up? stopping so a rerun can retry them".into());
                    break;
                }
            }
            Ok(text) => {
                consecutive_errs = 0;
                if !text.is_empty() {
                    with_text += 1;
                }
                if store(db, c, o, want, &rel, &text, secs)? {
                    embedded += 1;
                }
            }
        }
        if n % 50 == 0 {
            let el = t0.elapsed().as_secs_f64();
            let rate = n as f64 / el;
            eprintln!(
                "  {n}/{total} · {:.2} s/img · ~{} min left",
                el / n as f64,
                ((total - n) as f64 / rate / 60.0) as u64
            );
        }
    }
    stop.store(true, Ordering::SeqCst);
    drop(rx);
    let _ = reader.join();
    println!(
        "ocr [{want}]: {n} images this run, {with_text} with text, {errs} errors{}, {:.2} s/img{}",
        if o.embed {
            format!(", {embedded} embedded as meme:text")
        } else {
            String::new()
        },
        if n > 0 {
            t0.elapsed().as_secs_f64() / n as f64
        } else {
            0.0
        },
        if n < total {
            " — stopped early; rerun to continue"
        } else {
            ""
        }
    );
    rc
}

fn run_tesseract(
    c: &Cfg,
    db: &Db,
    o: &Opts,
    want: &str,
    todo: Vec<(String, String, String)>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    if Command::new("tesseract")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        return Err("tesseract not installed (paru -S tesseract tesseract-data-eng)".into());
    }
    let total = todo.len();
    let todo = Arc::new(todo);
    let next = Arc::new(AtomicUsize::new(0));
    let root = Arc::new(c.clone());
    let (tx, rx) =
        mpsc::sync_channel::<(String, Result<(String, f64), String>)>(c.index_threads * 2);
    let mut handles = vec![];
    for _ in 0..c.index_threads.max(1) {
        let (todo, next, tx, root, stop) = (
            todo.clone(),
            next.clone(),
            tx.clone(),
            root.clone(),
            stop.clone(),
        );
        handles.push(std::thread::spawn(move || loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let k = next.fetch_add(1, Ordering::Relaxed);
            if k >= todo.len() {
                break;
            }
            let (_id, rel, format) = &todo[k];
            let t = Instant::now();
            // GIF/WebP/BMP go through a first-frame PNG in a temp file (tesseract cannot read GIF)
            let src = match root.file_path(rel) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send((rel.clone(), Err(e)));
                    continue;
                }
            };
            let (input, tmp) = if matches!(format.as_str(), "jpeg" | "png") {
                (src.clone(), None)
            } else {
                match image_bytes(&src, format) {
                    Ok(png) => {
                        let p = std::env::temp_dir()
                            .join(format!("memetag-ocr-{}-{k}.png", std::process::id()));
                        if std::fs::write(&p, png).is_ok() {
                            (p.clone(), Some(p))
                        } else {
                            (src.clone(), None)
                        }
                    }
                    Err(_) => (src.clone(), None),
                }
            };
            let out = Command::new("tesseract")
                .arg(&input)
                .args(["stdout", "--psm", "6", "-l", "eng"])
                .stderr(Stdio::null())
                .output();
            if let Some(p) = tmp {
                let _ = std::fs::remove_file(p);
            }
            let res = match out {
                Ok(o) if o.status.success() => Ok((
                    clean(&String::from_utf8_lossy(&o.stdout)),
                    t.elapsed().as_secs_f64(),
                )),
                Ok(o) => Err(format!("tesseract exit {}", o.status)),
                Err(e) => Err(e.to_string()),
            };
            if tx.send((rel.clone(), res)).is_err() {
                break;
            }
        }));
    }
    drop(tx);
    let (mut n, mut with_text, mut errs, mut embedded) = (0usize, 0usize, 0usize, 0usize);
    for (rel, res) in rx {
        n += 1;
        match res {
            Err(e) => {
                errs += 1;
                eprintln!("skip {rel}: {e}");
            }
            Ok((text, secs)) => {
                if !text.is_empty() {
                    with_text += 1;
                }
                if store(db, c, o, want, &rel, &text, secs)? {
                    embedded += 1;
                }
            }
        }
        if n % 200 == 0 {
            eprintln!("  {n}/{total}");
        }
    }
    for h in handles {
        let _ = h.join();
    }
    println!(
        "ocr [{want}]: {n} images this run, {with_text} with text, {errs} errors{} (threads {}){}",
        if o.embed {
            format!(", {embedded} embedded as meme:text")
        } else {
            String::new()
        },
        c.index_threads,
        if n < total {
            " — stopped early; rerun to continue"
        } else {
            ""
        }
    );
    Ok(())
}

/// Model output → stored text: strip HTML tags, collapse repeated lines, trim, drop a lone NONE, cap length —
/// and drop *degenerate* output altogether: on a picture with no words deepseek-ocr invents a fake table, "1. 1. 1. …",
/// or a numbered list, so anything that is mostly digits/punctuation or one line repeated is treated as "no text".
/// Drop HTML-style tags by the HTML tokenizer's own rule (without heuristic stripping): `<` opens a tag only when
/// the next character is an ASCII letter, `/` or `!` — so "<3", "a < b" and "<-" are text — and a tag that never closes is
/// emitted as text rather than swallowing the rest of the caption.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let opens = after
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '/' || c == '!')
            .unwrap_or(false);
        match if opens { after.find('>') } else { None } {
            Some(j) => {
                out.push(' ');
                rest = &after[j + 1..];
            }
            None => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}
pub fn clean_model(raw: &str) -> String {
    let no_tags = strip_tags(raw);
    let mut lines: Vec<String> = vec![];
    for line in no_tags.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() {
            continue;
        }
        if lines.last().map(|p| p == &l).unwrap_or(false) {
            continue;
        }
        lines.push(l);
    }
    // repeated-token spam ("1. 1. 1." / "01211 01211") → the unique-token ratio collapses
    let text = lines.join("\n");
    if text.eq_ignore_ascii_case("none") || text.eq_ignore_ascii_case("none.") || text.is_empty() {
        return String::new();
    }
    let toks: Vec<&str> = text.split_whitespace().collect();
    let uniq: std::collections::HashSet<&str> = toks.iter().copied().collect();
    if toks.len() >= 8 && uniq.len() * 4 < toks.len() {
        return String::new();
    }
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let total = text.chars().filter(|c| !c.is_whitespace()).count();
    let has_word = toks
        .iter()
        .any(|t| t.chars().filter(|c| c.is_alphabetic()).count() >= 3);
    if total > 0 && (letters * 10 < total * 3 || !has_word) {
        return String::new();
    }
    // a single run-together token ("CategoryValueTop100Bottom100") is a hallucinated table, not a caption
    if toks.len() == 1 && toks[0].chars().count() > 24 {
        return String::new();
    }
    let mut out = text;
    cap(&mut out, 4000);
    out
}
/// At most `n` bytes, cut on a character boundary: `String::truncate` panics inside a multi-byte character, and
/// 4000 bytes of Cyrillic or CJK is an ordinary wall of text (review, 2026-09-22).
fn cap(s: &mut String, n: usize) {
    if s.len() > n {
        let mut k = n;
        while !s.is_char_boundary(k) {
            k -= 1;
        }
        s.truncate(k);
    }
}

#[cfg(test)]
mod tests {
    use super::clean_model;
    #[test]
    fn degenerate_outputs_become_empty() {
        assert_eq!(
            clean_model("<table>CategoryValueTop100Bottom100</table>"),
            ""
        ); // fake table: one long token, no real word run
        assert_eq!(
            clean_model("- 1. 1. 1. 1. 1. 1. 1. 1. 1. 1. 1. 1. 1. 1. 1."),
            ""
        ); // repeated token spam
        assert_eq!(clean_model("1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12"), ""); // numbered list, no letters
        assert_eq!(clean_model("NONE"), "");
        assert_eq!(clean_model("THAT IS  \nBEAUTIFUL."), "THAT IS\nBEAUTIFUL.");
        assert_eq!(
            clean_model("MY FAVORITE SEX  \nPOSITION IS THE JFK  \n\nI SPLATTER ALL OVER HER"),
            "MY FAVORITE SEX\nPOSITION IS THE JFK\nI SPLATTER ALL OVER HER"
        );
        assert_eq!(
            clean_model("quikmeme.com\nquikmeme.com\nquikmeme.com"),
            "quikmeme.com"
        );
        // a long Cyrillic reading is cut on a character boundary, never inside one
        let wall: String = (0..900)
            .map(|i| format!("сло{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let cut = clean_model(&wall);
        assert!(cut.len() <= 4000 && cut.len() > 3990, "{}", cut.len());
        assert!(wall.starts_with(&cut));
        assert_eq!(super::clean(&wall).len(), cut.len());
    }
    #[test]
    fn angle_brackets_follow_the_html_tokenizer_rule() {
        assert_eq!(clean_model("i <3 cats\nso much"), "i <3 cats\nso much");
        assert_eq!(
            clean_model("if a < b then b > a, always"),
            "if a < b then b > a, always"
        );
        assert_eq!(clean_model("<td>ONE</td><td>TWO</td>"), "ONE TWO");
        assert_eq!(
            clean_model("<b oops the model forgot to close this tag"),
            "<b oops the model forgot to close this tag"
        );
        assert_eq!(
            clean_model("</br>arrows -> here <- there"),
            "arrows -> here <- there"
        );
    }
}

/// Collapse whitespace, drop tesseract's junk lines (lone symbols), cap length.
fn clean(raw: &str) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let l: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let letters = l.chars().filter(|c| c.is_alphanumeric()).count();
        if letters >= 2 && letters * 2 >= l.chars().count() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&l);
        }
    }
    cap(&mut out, 4000);
    out
}
