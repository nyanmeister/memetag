//! `memetag serve`: the grab grid as a web page, for a phone browser.
//!
//! Written 2026-10-04 (Claude, Fable 5.1) after the CLI, index and thumbnail cache were shown to work in
//! Termux. The server is std::net only, one thread per connection, `Connection: close`; a few routes over
//! files the index already names, so no path from the network ever reaches the filesystem unresolved.
//! Rows are loaded from the index once and reloaded when the index file's mtime changes (a `pull` on the
//! phone); the same `query::eval` as the CLI and the grid decides matches.
//!
//! The shape of the page (one `render`, two scripts, nothing else):
//! - `/` is server-rendered HTML: a search form, a grid of cached thumbnails newest first (`PAGE` per
//!   page), prev/next links. Every tile has three controls: the picture itself (`a.s`, covers the tile),
//!   a small **Share** link at bottom-left (`a.sh`) and ⤢ at bottom-right (`a.o`, the original via `/file`).
//! - Tapping the picture **copies** it: `TILE_SCRIPT` fetches `/png?f=<index path>` and writes it to the
//!   clipboard with the async Clipboard API (PNG only; the first frame of an animation). Needs a Chromium
//!   browser on Android (Cromite there); Firefox-family cannot write images.
//! - **Share** opens the native share sheet from the browser with the Web Share API (`navigator.share`
//!   with the original fetched from `/file` as a File). That is the only way the sheet can open while the
//!   browser is in front: Android 10+ aborts an activity started from the background, which is what
//!   `termux-share` run by this process is (logcat: "Abort background activity starts", measured 2026-10-04).
//! - Both links carry a real `href` to `/share?f=…`, the **no-script and desktop fallback**: this process
//!   runs `share_command` (`termux-share` on the phone, `memetag clip` elsewhere) and redirects back with a
//!   message. The script lets Share follow that link when the browser has no `navigator.share` (desktop
//!   Chromium on Linux, Firefox), and reports instead of following when the API exists but refuses the file.
//! - `/thumb` serves the cache (`grab::fresh_thumb`, or a strip's first frame cut once), `/suggest` answers
//!   `SUGGEST_SCRIPT` with completions, `/log` is a beacon the page sends its toast text to (the only way
//!   to read the browser's verdict from the shell), `/manifest.webmanifest` + `/icon.png` make the
//!   home-screen icon, `/share` is the fallback above.
use crate::index::{Db, FileRow};
use crate::{autocomplete, grab, query, similar, sources, Cfg};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

/// A page request starts a background pull when the last one is older than this.
const PULL_EVERY_SECS: u64 = 300;

const PAGE: usize = 60;
const TILE_H: u32 = 128;

struct Shared {
    c: Cfg,
    share_command: String,
    share_stage: bool,
    /// Run once when an original is missing (the optional config.toml `mount_command`).
    mount_command: Option<String>,
    rows: Mutex<(Option<SystemTime>, Arc<Vec<FileRow>>)>,
    /// Tags the desktop grid would offer too (tag_history, aliases, implications), merged with live counts in `tags()`.
    remembered: Vec<(String, u64, bool)>,
    tags: Mutex<(Option<SystemTime>, Arc<Vec<(String, u64, bool)>>)>,
    /// (when the last pull finished, whether one is running now)
    pull: Mutex<(Option<Instant>, bool)>,
}

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let (mut bind, mut share) = ("127.0.0.1:7777".to_string(), None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bind" => bind = it.next().ok_or("--bind needs ADDR:PORT")?.clone(),
            "--share" => share = Some(it.next().ok_or("--share needs a command")?.clone()),
            other => return Err(format!("serve: unknown argument {other}")),
        }
    }
    // config.toml: `share_command` ("{file}" is replaced), `share_stage` (copy into the cache first),
    // `mount_command` (run when an original is missing, before giving up).
    let table = crate::paths::config_table()?;
    let text = |k: &str| table.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let (cfg_share, mount_command) = (text("share_command"), text("mount_command"));
    let cfg_stage = table.get("share_stage").and_then(|v| v.as_bool());
    let termux = crate::companion("termux-share").is_ok();
    let share_command = share.or(cfg_share).unwrap_or_else(|| {
        if termux {
            "termux-share -a send {file}".into()
        } else {
            format!(
                "{} clip {{file}}",
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or("memetag".into())
            )
        }
    });
    // Termux:API runs outside Termux's mount namespace and cannot read a FUSE mount made here (measured 2026-10-04),
    // so by default on Android the pick is copied under the cache first; a desktop clip wants the real path.
    let share_stage = cfg_stage.unwrap_or(termux);
    let listener = TcpListener::bind(&bind).map_err(|e| format!("serve: bind {bind}: {e}"))?;
    let db = Db::open_cfg(c)?;
    let rows = db.all()?;
    eprintln!(
        "serve: http://{bind}/  {} files, share: {share_command}{}",
        rows.len(),
        if share_stage { " (staged copy)" } else { "" }
    );
    let shared = Arc::new(Shared {
        c: c.clone(),
        share_command,
        share_stage,
        mount_command,
        rows: Mutex::new((index_mtime(&c.db), Arc::new(rows))),
        remembered: autocomplete::load(c)
            .into_iter()
            .filter(|(_, _, own)| *own)
            .collect(),
        tags: Mutex::new((None, Arc::new(vec![]))),
        pull: Mutex::new((None, false)),
    });
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let s = shared.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(&s, stream) {
                eprintln!("serve: {e}");
            }
        });
    }
    Ok(())
}

fn index_mtime(db: &Path) -> Option<SystemTime> {
    std::fs::metadata(db).and_then(|m| m.modified()).ok()
}

/// The rows, reloaded when the index file changed since they were read.
fn rows(s: &Shared) -> Result<Arc<Vec<FileRow>>, String> {
    let now = index_mtime(&s.c.db);
    let mut g = s.rows.lock().map_err(|_| "rows lock poisoned")?;
    if g.0 != now {
        let db = Db::open_cfg(&s.c)?;
        *g = (now, Arc::new(db.all()?));
    }
    Ok(g.1.clone())
}

/// Start a background pull when none is running and the last finished more than `PULL_EVERY_SECS` ago, so a meme
/// saved on the desktop shows up here without anyone remembering to run `memetag pull`; the grid does the same on
/// launch. The page itself is served at once from the index as it is; rows reload when the pull has written.
fn maybe_pull(s: &Arc<Shared>) {
    {
        let Ok(mut g) = s.pull.lock() else { return };
        let due = g.0.is_none_or(|t| t.elapsed().as_secs() >= PULL_EVERY_SECS);
        if g.1 || !due {
            return;
        }
        g.1 = true;
    }
    let s = s.clone();
    std::thread::spawn(move || {
        // thumbnails for new files read the originals: bring the mount up first when a command is configured
        if let (Some(cmd), false) = (
            &s.mount_command,
            s.c.root
                .join(".")
                .read_dir()
                .map(|mut d| d.next().is_some())
                .unwrap_or(false),
        ) {
            let _ = std::process::Command::new("sh").arg("-c").arg(cmd).status();
        }
        let o = crate::pull::for_serve(&s.c);
        eprintln!(
            "serve: pull {} written, {} gone, {} failed",
            o.written.len(),
            o.gone.len(),
            o.failed
        );
        if let Ok(mut g) = s.pull.lock() {
            *g = (Some(Instant::now()), false);
        }
    });
}

/// The completion vocabulary: live tag counts over the rows, merged with the remembered tags, as the desktop grid does.
/// Rebuilt when the rows were (same mtime key).
fn tags(s: &Shared) -> Result<Arc<Vec<(String, u64, bool)>>, String> {
    let rows = rows(s)?;
    let now = index_mtime(&s.c.db);
    let mut g = s.tags.lock().map_err(|_| "tags lock poisoned")?;
    if g.0 != now || g.1.is_empty() {
        let values = autocomplete::local_counts(&rows, &s.remembered);
        *g = (now, Arc::new(values));
    }
    Ok(g.1.clone())
}

/// Completions for the tag under the caret (`c` counts characters, as the grid's does): each one as the whole
/// query with that tag quoted in, plus where the caret lands after it.
fn suggest(s: &Shared, q: &str, caret: usize) -> Result<String, String> {
    let Some((span, prefix)) = query::completion(q, caret) else {
        return Ok("[]".into());
    };
    let values = tags(s)?;
    let items: Vec<serde_json::Value> = autocomplete::matches(&values, &prefix)
        .into_iter()
        .map(|tag| {
            let replacement = query::quote_tag(&tag);
            let mut full = q.to_string();
            full.replace_range(span.clone(), &replacement);
            let after = q[..span.start].chars().count() + replacement.chars().count();
            serde_json::json!({"t": tag, "q": full, "c": after})
        })
        .collect();
    serde_json::to_string(&items).map_err(|e| e.to_string())
}

struct Req {
    path: String,
    query: HashMap<String, String>,
}

fn parse_request(stream: &mut TcpStream) -> Result<Req, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        let n = stream.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16384 {
            break;
        }
    }
    let line = buf.split(|&b| b == b'\n').next().ok_or("empty request")?;
    let line = String::from_utf8_lossy(line);
    let mut parts = line.split_whitespace();
    let method = parts.next().ok_or("no method")?;
    let target = parts.next().ok_or("no target")?;
    if method != "GET" && method != "HEAD" {
        return Err(format!("method {method} not supported"));
    }
    let (path, qs) = target.split_once('?').unwrap_or((target, ""));
    let query = qs
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Ok(Req {
        path: percent_decode(path),
        query,
    })
}

fn handle(s: &Arc<Shared>, mut stream: TcpStream) -> Result<(), String> {
    let req = match parse_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            return respond(
                &mut stream,
                400,
                "text/plain; charset=utf-8",
                e.as_bytes(),
                &[],
            )
        }
    };
    let q = req.query.get("q").cloned().unwrap_or_default();
    let page: usize = req
        .query
        .get("p")
        .and_then(|p| p.parse().ok())
        .unwrap_or(1)
        .max(1);
    let f = req.query.get("f").cloned().unwrap_or_default();
    if req.path != "/thumb" {
        // One line per request except thumbnails (sixty a page): what a browser actually asked for.
        eprintln!(
            "serve: {}{}{}",
            req.path,
            if f.is_empty() { "" } else { " " },
            f
        );
    }
    match req.path.as_str() {
        "/" => {
            maybe_pull(s);
            match render(s, &q, page, req.query.get("msg").map(String::as_str)) {
                Ok(html) => respond(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    html.as_bytes(),
                    &[],
                ),
                Err(e) => respond(
                    &mut stream,
                    500,
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                    &[],
                ),
            }
        }
        "/thumb" => {
            let Some(row) = find(s, &f)? else {
                return not_found(&mut stream);
            };
            let pic = thumb(s, &row).or_else(|| {
                // Not in the cache (a file the last pull brought in while the mount was down): make it now, as the
                // grid does on demand, which needs the original.
                s.c.file_path(&row.path)
                    .ok()
                    .filter(|p| reachable(s, p).is_ok())
                    .and_then(|_| grab::ensure_thumbs(&s.c, &[&row]).pop())
                    .map(|(p, _)| (p, 1.0))
            });
            match pic {
                Some((p, _)) => send_file(&mut stream, &p, "image/png", "max-age=86400"),
                None => not_found(&mut stream),
            }
        }
        "/file" => {
            let Some(row) = find(s, &f)? else {
                return not_found(&mut stream);
            };
            match original_bytes(s, &row) {
                Ok(bytes) => respond(
                    &mut stream,
                    200,
                    mime(&row.format),
                    &bytes,
                    &[("Cache-Control", "no-cache")],
                ),
                Err(e) => unavailable(&mut stream, &e),
            }
        }
        "/png" => {
            let Some(row) = find(s, &f)? else {
                return not_found(&mut stream);
            };
            match png_of(s, &row) {
                Ok(bytes) => respond(
                    &mut stream,
                    200,
                    "image/png",
                    &bytes,
                    &[("Cache-Control", "no-cache")],
                ),
                Err(e) => unavailable(&mut stream, &e),
            }
        }
        "/suggest" => {
            let caret = req
                .query
                .get("c")
                .and_then(|v| v.parse().ok())
                .unwrap_or(q.chars().count());
            match suggest(s, &q, caret) {
                Ok(json) => respond(
                    &mut stream,
                    200,
                    "application/json",
                    json.as_bytes(),
                    &[("Cache-Control", "no-store")],
                ),
                Err(e) => respond(
                    &mut stream,
                    500,
                    "text/plain; charset=utf-8",
                    e.as_bytes(),
                    &[],
                ),
            }
        }
        "/log" => {
            eprintln!(
                "serve: page says: {}",
                req.query.get("m").map(String::as_str).unwrap_or("")
            );
            respond(&mut stream, 204, "text/plain", b"", &[])
        }
        "/manifest.webmanifest" => respond(
            &mut stream,
            200,
            "application/manifest+json",
            MANIFEST.as_bytes(),
            &[("Cache-Control", "max-age=86400")],
        ),
        "/icon.png" => {
            let size = req
                .query
                .get("s")
                .and_then(|v| v.parse().ok())
                .unwrap_or(192u32)
                .clamp(48, 1024);
            respond(
                &mut stream,
                200,
                "image/png",
                &icon_png(size),
                &[("Cache-Control", "max-age=86400")],
            )
        }
        "/share" => {
            let Some(row) = find(s, &f)? else {
                return not_found(&mut stream);
            };
            let msg = match share(s, &row) {
                Ok(()) => format!("shared {}", basename(&row.path)),
                Err(e) => format!("share failed: {e}"),
            };
            let back = format!(
                "/?q={}&p={page}&msg={}",
                percent_encode(&q),
                percent_encode(&msg)
            );
            respond(
                &mut stream,
                303,
                "text/plain; charset=utf-8",
                b"",
                &[("Location", &back)],
            )
        }
        _ => not_found(&mut stream),
    }
}

/// Web app manifest, so "Add to Home screen" in the phone browser installs the grid as a standalone icon.
const MANIFEST: &str = r##"{"name":"memetag","short_name":"memes","start_url":"/","display":"standalone","background_color":"#111111","theme_color":"#111111","icons":[{"src":"/icon.png?s=192","sizes":"192x192","type":"image/png"},{"src":"/icon.png?s=512","sizes":"512x512","type":"image/png"}]}"##;

/// A drawn icon (no file to ship): a green rounded square with a lighter "tile" in it.
fn icon_png(size: u32) -> Vec<u8> {
    let s = size as f32;
    let r = s * 0.2;
    let inside_round = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, rad: f32| {
        let cx = x.clamp(x0 + rad, x1 - rad);
        let cy = y.clamp(y0 + rad, y1 - rad);
        (x - cx).hypot(y - cy) <= rad
    };
    let img = image::RgbaImage::from_fn(size, size, |px, py| {
        let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
        if inside_round(x, y, s * 0.3, s * 0.3, s * 0.7, s * 0.7, s * 0.06) {
            image::Rgba([245, 250, 245, 255])
        } else if inside_round(x, y, 0.0, 0.0, s, s, r) {
            image::Rgba([34, 170, 85, 255])
        } else {
            image::Rgba([0, 0, 0, 0])
        }
    });
    let mut out = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut out, image::ImageFormat::Png);
    out.into_inner()
}

/// The original as PNG for the browser's clipboard (the async Clipboard API takes PNG only): a PNG file as it is,
/// anything else decoded (an animation's first frame) and encoded, longest side capped at 2048 px; a video or an
/// undecodable file falls back to its cached thumbnail.
fn png_of(s: &Shared, r: &FileRow) -> Result<Vec<u8>, String> {
    let decoded = match original_bytes(s, r) {
        Ok(bytes) if r.format == "png" => return Ok(bytes),
        Ok(bytes) => image::load_from_memory(&bytes).ok(),
        Err(_) => None,
    };
    let img = match decoded {
        Some(i) => i,
        None => {
            let (t, _) = thumb(s, r).ok_or_else(|| format!("{}: no picture to copy", r.path))?;
            return std::fs::read(&t).map_err(|e| format!("{}: {e}", t.display()));
        }
    };
    let img = if img.width().max(img.height()) > 2048 {
        img.resize(2048, 2048, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("{}: png encode: {e}", r.path))?;
    Ok(out.into_inner())
}

/// Use the authoritative SSH reader for configured network sources, avoiding a
/// disconnected or stale FUSE mount. It rejects incomplete transfers. Local
/// collections still read their local files; thumbnails remain in the cache.
fn original_bytes(s: &Shared, r: &FileRow) -> Result<Vec<u8>, String> {
    let p = s.c.file_path(&r.path)?;
    crate::batch::read_for_view(&s.c, &p)
}

fn find(s: &Shared, key: &str) -> Result<Option<FileRow>, String> {
    if key.is_empty() {
        return Ok(None);
    }
    Ok(rows(s)?.iter().find(|r| r.path == key).cloned())
}

/// Cached picture for a row: (path, width/height). For an animated file it is the storyboard's first frame,
/// cut once into `<key>.strip.first.png` beside the strip: a strip averages 1.1 MB against 69 KB for a still
/// (measured over this collection's cache, 2026-10-04), and a page shows sixty of them.
fn thumb(s: &Shared, r: &FileRow) -> Option<(PathBuf, f64)> {
    if let Some(p) = grab::fresh_thumb(&s.c, r) {
        let (w, h) = png_size(&p)?;
        return Some((p, w as f64 / h.max(1) as f64));
    }
    let (strip, n, _dur) = grab::cached_strip(&s.c, r)?;
    let first = strip.with_extension("first.png");
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    if mtime(&first).zip(mtime(&strip)).is_none_or(|(f, s)| f < s) {
        let img = image::open(&strip).ok()?;
        let fw = (img.width() / n.max(1)).max(1);
        let tmp = first.with_extension("tmp.png");
        img.crop_imm(0, 0, fw, img.height()).save(&tmp).ok()?;
        std::fs::rename(&tmp, &first).ok()?;
    }
    let (w, h) = png_size(&first)?;
    Some((first, w as f64 / h.max(1) as f64))
}

/// The original must be readable here; when it is not and a `mount_command` is configured, run it once and look again.
fn reachable(s: &Shared, p: &Path) -> Result<(), String> {
    if p.exists() {
        return Ok(());
    }
    let Some(cmd) = &s.mount_command else {
        return Err(format!(
            "{} is not there (is the collection mounted?)",
            p.display()
        ));
    };
    let st = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .status()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !st.success() {
        return Err(format!("{cmd}: exit {st}"));
    }
    if p.exists() {
        Ok(())
    } else {
        Err(format!("{} still missing after `{cmd}`", p.display()))
    }
}

fn share(s: &Shared, r: &FileRow) -> Result<(), String> {
    let src = s.c.file_path(&r.path)?;
    reachable(s, &src)?;
    let file = if s.share_stage {
        let out = s.c.thumbs.parent().unwrap_or(&s.c.thumbs).join("out");
        std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
        prune(&out);
        let dst = out.join(src.file_name().ok_or("no file name")?);
        std::fs::copy(&src, &dst).map_err(|e| format!("copy {}: {e}", src.display()))?;
        dst
    } else {
        src
    };
    let quoted = format!("'{}'", file.display().to_string().replace('\'', "'\\''"));
    let cmd = s.share_command.replace("{file}", &quoted);
    let st = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .status()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if st.success() {
        Ok(())
    } else {
        Err(format!("{cmd}: exit {st}"))
    }
}

/// Staged copies older than an hour go; the share sheet has long consumed them.
fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() > 3600);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn render(s: &Shared, q: &str, page: usize, msg: Option<&str>) -> Result<String, String> {
    let rows = rows(s)?;
    let c = &s.c;
    let mut hits: Vec<&FileRow> = if q.trim().is_empty() {
        rows.iter().collect()
    } else {
        let mut expr = query::parse(q)?;
        sources::resolve_query(c, &mut expr)?;
        let alias = |t: &str| c.vocab.canon(t);
        let wanted = query::similar_terms(&expr);
        let mut lookup = similar::Lookup::new(if wanted.is_empty() {
            Default::default()
        } else {
            let db = Db::open_cfg(c)?;
            similar::ensure(c, &db, rows.as_slice(), false)?
        });
        if let Some(e) = lookup.prepare(&wanted).into_iter().next() {
            return Err(e);
        }
        let sim = |v: &str, p: &str| lookup.distance(v, p);
        rows.iter()
            .filter(|f| query::eval(&expr, &query::Item::of(f, &sim), &alias))
            .collect()
    };
    // Newest first, as the grid shows them.
    hits.sort_by(|a, b| {
        b.created_at
            .partial_cmp(&a.created_at)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let total = hits.len();
    let pages = total.div_ceil(PAGE).max(1);
    let page = page.min(pages);
    let slice = &hits[(page - 1) * PAGE..((page) * PAGE).min(total)];
    let qe = esc(q);
    let qp = percent_encode(q);
    let mut h = String::with_capacity(16384);
    h.push_str("<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>memetag</title><link rel=manifest href=/manifest.webmanifest><meta name=theme-color content=#111111><link rel=icon href=/icon.png?s=192><style>");
    h.push_str(":root{color-scheme:light dark}body{margin:0;font:15px system-ui,sans-serif;background:#111;color:#ddd}@media(prefers-color-scheme:light){body{background:#f4f4f4;color:#222}}");
    h.push_str("form{display:flex;gap:6px;padding:8px;position:sticky;top:0;background:inherit}input[type=search]{flex:1;font:inherit;padding:8px;border-radius:6px;border:1px solid #666;background:inherit;color:inherit}button{font:inherit;padding:8px 12px;border-radius:6px;border:1px solid #666;background:inherit;color:inherit}");
    h.push_str(".n{padding:0 10px 6px;opacity:.7;font-size:13px}.g{display:flex;flex-wrap:wrap;gap:4px;padding:4px}.t{position:relative;height:");
    h.push_str(&TILE_H.to_string());
    h.push_str("px;background:#000 center/cover no-repeat;border-radius:4px;flex-grow:1}.t a.s{position:absolute;inset:0}.t a.o,.t a.sh{position:absolute;bottom:2px;padding:3px 7px;font-size:13px;background:#000a;color:#fff;border-radius:4px;text-decoration:none}.t a.o{right:2px}.t a.sh{left:2px}");
    h.push_str(".p{display:flex;justify-content:space-between;padding:12px}.p a{color:inherit}.m{padding:6px 10px;background:#2a5;color:#fff}#toast,#transfer{position:fixed;left:8px;right:8px;bottom:12px;border-radius:6px;z-index:3}#toast{text-align:center}#transfer span{display:block}#transfer button{margin:4px 4px 0 0}[hidden]{display:none}#sug{display:flex;flex-direction:column;gap:3px;padding:0 8px 6px}#sug button{text-align:left;padding:9px 10px;border-radius:6px;border:1px solid #555;background:inherit;color:inherit;font:inherit}#sug small{opacity:.6;float:right}");
    h.push_str("</style></head><body>");
    h.push_str("<form action=\"/\"><input type=search name=q placeholder=\"tags, t:text, format:gif …\" value=\"");
    h.push_str(&qe);
    h.push_str("\"><button>Search</button></form><div id=sug></div>");
    if let Some(m) = msg {
        h.push_str("<div class=m>");
        h.push_str(&esc(m));
        h.push_str("</div>");
    }
    h.push_str(&format!("<div class=n>{total} of {} files · page {page} of {pages} · tap a picture to copy it · Share opens the share sheet · ⤢ opens the original</div><div class=g>", rows.len()));
    for r in slice {
        let fe = percent_encode(&r.path);
        let anim = if animated(r) { " data-anim=1" } else { "" };
        // The picture copies, Share shares; both links also work without script (the server's /share).
        let controls = format!(
            "<a class=s href=\"/share?f={fe}&q={qp}&p={page}\" data-f=\"{fe}\"{anim} title=\"{0}\"></a><a class=sh href=\"/share?f={fe}&q={qp}&p={page}\" data-f=\"{fe}\" data-n=\"{0}\">Share</a><a class=o href=\"/file?f={fe}\">⤢</a>",
            esc(basename(&r.path))
        );
        let Some((_, aspect)) = thumb(s, r) else {
            // No cached picture (a file pull has not thumbnailed yet): a labelled box with the same controls.
            h.push_str(&format!(
                "<div class=t style=\"width:{}px;display:flex;align-items:center;justify-content:center;font-size:12px;overflow:hidden\">{}{controls}</div>",
                TILE_H, esc(basename(&r.path))
            ));
            continue;
        };
        let w = (TILE_H as f64 * aspect).round().max(48.0) as u32;
        h.push_str(&format!(
            "<div class=t style=\"width:{w}px;background:#000 url('/thumb?f={fe}') center/cover no-repeat\">{controls}</div>"
        ));
    }
    h.push_str("</div><div class=p>");
    if page > 1 {
        h.push_str(&format!("<a href=\"/?q={qp}&p={}\">← newer</a>", page - 1));
    } else {
        h.push_str("<span></span>");
    }
    if page < pages {
        h.push_str(&format!("<a href=\"/?q={qp}&p={}\">older →</a>", page + 1));
    }
    h.push_str("</div><div id=toast class=m hidden></div>");
    h.push_str(TILE_SCRIPT);
    h.push_str(SUGGEST_SCRIPT);
    h.push_str("</body></html>");
    Ok(h)
}

/// The tile script: the picture copies, Share shares. Both are the browser's own calls, which is the point: a
/// browser puts an image on the Android clipboard only from a page's call to the async Clipboard API (PNG
/// only, user gesture, secure context — localhost counts), and the share sheet can only be opened by the app
/// in front, which is the browser, through the Web Share API (`navigator.share` with files; Chromium on
/// Android, not Firefox). Modern ClipboardItem accepts a promise, so copying starts during the tap and
/// waits for a complete download. Older browsers use a prepared-file button after activation expires;
/// sharing uses that same button when necessary. Retry only downloads, never native browser actions.
/// Share without `navigator.share` follows its link to the server's `/share` (desktop clip); with it but a
/// refused file (`canShare` false: a type Chromium does not share, e.g. 7z) it says so instead.
const TILE_SCRIPT: &str = concat!("<script>\n", include_str!("web_actions.js"), "</script>");

/// Tag suggestions while typing: `/suggest` answers for the tag under the caret, with the whole query each
/// completion would produce, so a tap sets the field and runs the search (one tap = results on a phone; another
/// tag is added by typing into the field again). Debounced; nothing is fetched for an empty prefix.
const SUGGEST_SCRIPT: &str = r#"<script>
(function(){var i=document.querySelector('input[name=q]'),box=document.getElementById('sug'),timer,last='';
function caret(){return Array.from(i.value.slice(0,i.selectionStart)).length}
function show(list){box.textContent='';list.forEach(function(it){var b=document.createElement('button');b.type='button';b.textContent=it.t;b.onclick=function(){i.value=it.q;var u=Array.from(it.q).slice(0,it.c).join('').length;i.setSelectionRange(u,u);last=i.value+'\u0000'+caret();show([]);i.form.submit()};box.appendChild(b)})}
function ask(){clearTimeout(timer);timer=setTimeout(function(){var key=i.value+'\0'+caret();if(key===last)return;last=key;
fetch('/suggest?q='+encodeURIComponent(i.value)+'&c='+caret()).then(function(r){return r.json()}).then(show,function(){show([])})},120)}
i.addEventListener('input',ask);i.addEventListener('click',ask);i.addEventListener('keyup',function(e){if(e.key==='ArrowLeft'||e.key==='ArrowRight')ask()});
})();
</script>"#;

fn animated(r: &FileRow) -> bool {
    r.kind == "video" || matches!(r.format.as_str(), "gif" | "webp" | "apng")
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn mime(format: &str) -> &'static str {
    match format {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "apng" => "image/apng",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "mp4" => "video/mp4",
        "webm" | "mkv" => "video/webm",
        "mp3" => "audio/mpeg",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Width and height from a PNG's IHDR, without decoding it.
fn png_size(p: &Path) -> Option<(u32, u32)> {
    let mut f = std::fs::File::open(p).ok()?;
    let mut head = [0u8; 24];
    f.read_exact(&mut head).ok()?;
    if &head[..8] != b"\x89PNG\r\n\x1a\n" || &head[12..16] != b"IHDR" {
        return None;
    }
    let be = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    Some((be(&head[16..20]), be(&head[20..24])))
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    ctype: &str,
    body: &[u8],
    extra: &[(&str, &str)],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        303 => "See Other",
        400 => "Bad Request",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|e| e.to_string())
}

fn not_found(stream: &mut TcpStream) -> Result<(), String> {
    respond(stream, 404, "text/plain; charset=utf-8", b"not found", &[])
}

fn unavailable(stream: &mut TcpStream, message: &str) -> Result<(), String> {
    respond(
        stream,
        503,
        "text/plain; charset=utf-8",
        message.as_bytes(),
        &[("Cache-Control", "no-store"), ("Retry-After", "2")],
    )
}

fn send_file(stream: &mut TcpStream, p: &Path, ctype: &str, cache: &str) -> Result<(), String> {
    match std::fs::read(p) {
        Ok(bytes) => respond(stream, 200, ctype, &bytes, &[("Cache-Control", cache)]),
        Err(e) => respond(
            stream,
            404,
            "text/plain; charset=utf-8",
            format!("{}: {e}", p.display()).as_bytes(),
            &[],
        ),
    }
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn percent_encode(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut o = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    o.push(v);
                    i += 3;
                    continue;
                }
                o.push(b'%');
                i += 1;
            }
            b'+' => {
                o.push(b' ');
                i += 1;
            }
            c => {
                o.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&o).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percent_round_trip() {
        for s in ["main/Old/a b.gif", "t:ǒ, cat || dog", "100%", "%zz", "a+b"] {
            let enc = percent_encode(s);
            assert_eq!(percent_decode(&enc), s, "{s}");
        }
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("x+y"), "x y");
    }
    #[test]
    fn escapes_html() {
        assert_eq!(
            esc("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }
}
