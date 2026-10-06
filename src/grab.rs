//! The point of the whole thing: a clickable grid of thumbnails; a click puts the full-size file on the clipboard.
//!
//! Cache naming: files are keyed by the **relative path** (`p` + 16 hex of its SHA-256),
//! not by the file id — the id changes from the provisional byte hash to the pixel hash the first time a file is tagged,
//! which used to throw every newly tagged image's thumbnail away. Freshness is judged against the mtime the index
//! recorded, so drawing the grid never stats the originals over the mount. `migrate_cache` renames a cache built under
//! the old scheme once, in place, using the index to map ids back to paths.
use crate::index::{Db, FileRow};
use crate::Cfg;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

pub const STRIP_SIZE: u32 = 256;
const MIGRATED_MARKER: &str = ".keyed-by-path";

/// Drop every cache file for a path the index no longer has (pull, after a delete or rename).
pub fn forget(c: &Cfg, rel: &str) {
    let k = library_cache_key(c, rel);
    for p in [
        thumb_path(c, &k),
        still_marker(c, &k),
        strip_png(c, &k),
        strip_meta(c, &k),
    ] {
        let _ = std::fs::remove_file(p);
    }
}
pub fn cache_key(rel: &str) -> String {
    format!("p{}", hex::encode(&Sha256::digest(rel.as_bytes())[..8]))
}
pub fn library_cache_key(c: &Cfg, key: &str) -> String {
    if c.library().ok().flatten().is_some() {
        if let Some(rel) = key.strip_prefix("main/") {
            return cache_key(rel);
        }
        return cache_key(&format!("\0source\0{key}"));
    }
    cache_key(key)
}
fn thumb_path(c: &Cfg, key: &str) -> PathBuf {
    c.thumbs.join(format!("{key}.png"))
}
fn still_marker(c: &Cfg, key: &str) -> PathBuf {
    c.thumbs.join(format!("{key}.still"))
}
fn strip_png(c: &Cfg, key: &str) -> PathBuf {
    c.thumbs.join(format!("{key}.strip.png"))
}
fn strip_meta(c: &Cfg, key: &str) -> PathBuf {
    c.thumbs.join(format!("{key}.strip.txt"))
}
/// A cached file is fresh when it is at least as new as the source's mtime **as indexed** (no stat over the mount).
fn fresh(p: &Path, r: &FileRow) -> bool {
    p.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() + 1.0 >= r.mtime)
        .unwrap_or(false)
}
/// The cached still thumbnail for this row when it is fresh (the perceptual hasher reads it instead of the original).
pub fn fresh_thumb(c: &Cfg, r: &FileRow) -> Option<PathBuf> {
    let p = thumb_path(c, &library_cache_key(c, &r.path));
    fresh(&p, r).then_some(p)
}
/// The cached strip for this row if it is fresh and was made for the configured frame budget: (path, frames, duration_s).
fn strip_cached(c: &Cfg, key: &str, r: &FileRow) -> Option<(PathBuf, u32, f64)> {
    let png = strip_png(c, key);
    if !fresh(&png, r) {
        return None;
    }
    let meta = std::fs::read_to_string(strip_meta(c, key)).ok()?;
    let mut it = meta.split_whitespace();
    let (n, want, ms): (u32, u32, u64) = (
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    );
    if want != c.strip_frames {
        return None;
    }
    Some((png, n, ms as f64 / 1000.0))
}

/// The storyboard already in the cache for this row, if any: (path, frames, duration_s). Never builds one
/// (`serve` reads the cache a `pull` filled; a missing strip is shown as a labelled box there).
pub fn cached_strip(c: &Cfg, r: &FileRow) -> Option<(PathBuf, u32, f64)> {
    let key = library_cache_key(c, &r.path);
    if still_marker(c, &key).exists() {
        return None;
    }
    strip_cached(c, &key, r)
}

/// YouTube-style storyboard for a video or animated GIF/WebP/APNG: (path, frames, real duration in seconds).
/// Frames are sampled evenly across the whole clip; the UI plays them back at frames/duration
/// (the clip's own speed, so a 10 fps GIF runs at 10 fps), capped at `preview_fps`.
pub fn ensure_strip(c: &Cfg, r: &FileRow) -> Option<(PathBuf, u32, f64)> {
    let animated_image = matches!(r.format.as_str(), "gif" | "webp" | "apng");
    if r.kind != "video" && !animated_image {
        return None;
    }
    let key = library_cache_key(c, &r.path);
    if animated_image && still_marker(c, &key).exists() {
        return None;
    }
    if let Some(hit) = strip_cached(c, &key, r) {
        return Some(hit);
    }
    let src = c.file_path(&r.path).ok()?;
    let mut want = c.strip_frames;
    let mut dur = 0.0;
    if animated_image {
        // One probe for frame count and length: animated WebP has no container duration (ffprobe says N/A,
        // 2026-09-16), so the packets are the only clock there is; for GIF they agree with the header.
        let probed = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time,duration_time",
                "-of",
                "csv=p=0",
            ])
            .arg(&src)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| packet_timing(&s));
        let (frames, end) = match probed {
            // the marker is checked by existence alone, so only a probe that saw the one packet may write it: a
            // failed or empty probe (ffprobe missing, the mount stalled) used to mark an animation a still for good
            Some((1, _)) => {
                let _ = std::fs::write(still_marker(c, &key), b"");
                return None;
            }
            Some((0, _)) | None => return None,
            Some(x) => x,
        };
        want = want.min(frames).max(2); // never sample more frames than the GIF has → native frames, native speed
        dur = end;
    }
    if dur <= 0.0 {
        dur = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(&src)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0.0);
    }
    if dur <= 0.0 {
        return None;
    }
    let n = if r.kind == "video" && dur < 1.0 {
        (want / 2).max(2)
    } else {
        want
    };
    let sp = strip_png(c, &key);
    // no padding: every frame keeps the clip's aspect ratio; frame count and duration go in the sidecar
    let vf = format!(
        "fps={n}/{dur:.3},scale={s}:{s}:force_original_aspect_ratio=decrease,tile={n}x1",
        s = STRIP_SIZE
    );
    let tmp = scratch(&sp);
    let ok = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(&src)
        .args(["-vf", &vf, "-frames:v", "1"])
        .arg(&tmp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || std::fs::rename(&tmp, &sp).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    std::fs::write(
        strip_meta(c, &key),
        format!("{n} {} {}\n", c.strip_frames, (dur * 1000.0).round() as u64),
    )
    .ok()?;
    Some((sp, n, dur))
}

/// (packet count, end of the last packet in seconds) from ffprobe's `packet=pts_time,duration_time` CSV.
/// A field ffprobe prints as N/A counts as zero, so one odd packet cannot sink the whole clip.
fn packet_timing(csv: &str) -> (u32, f64) {
    let num = |v: Option<&str>| v.and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(0.0);
    csv.lines()
        .filter(|l| !l.trim().is_empty())
        .fold((0, 0.0), |(n, end), line| {
            let mut it = line.split(',');
            let (pts, d) = (num(it.next()), num(it.next()));
            (n + 1, f64::max(end, pts + d))
        })
}

/// Where a cache file is written before it is renamed into place: a worker killed mid-save (the window closing during
/// a fresh build) used to leave a truncated PNG under the final name with a new mtime, fresh forever and undecodable.
fn scratch(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = name.strip_suffix(".png").unwrap_or(&name);
    target.with_file_name(format!("{stem}.tmp{}.png", std::process::id()))
}
/// Make sure every row has a thumbnail; returns (thumb, row) for the ones that could be made.
pub fn ensure_thumbs<'a>(c: &Cfg, rows: &[&'a FileRow]) -> Vec<(PathBuf, &'a FileRow)> {
    std::fs::create_dir_all(&c.thumbs).ok();
    let mut out = vec![];
    for r in rows {
        let tp = thumb_path(c, &library_cache_key(c, &r.path));
        if !fresh(&tp, r) {
            let Ok(src) = c.file_path(&r.path) else {
                continue;
            };
            let tmp = scratch(&tp);
            let ok = if r.kind == "video" {
                video_still(&src, &tmp)
            } else {
                std::fs::read(&src)
                    .ok()
                    .and_then(|b| crate::containers::decode(&b).ok())
                    .map(|img| img.thumbnail(256, 256).save(&tmp).is_ok())
                    .unwrap_or(false)
            };
            if !ok || std::fs::rename(&tmp, &tp).is_err() {
                let _ = std::fs::remove_file(&tmp);
                continue;
            }
        }
        out.push((tp, *r));
    }
    out
}

/// One still frame from a video: ffmpegthumbnailer when it is installed, otherwise ffmpeg's `thumbnail` filter
/// (picks a representative frame out of the first 50, no seeking, so short clips work too).
fn video_still(src: &Path, tp: &Path) -> bool {
    let a = Command::new("ffmpegthumbnailer")
        .args(["-i"])
        .arg(src)
        .arg("-o")
        .arg(tp)
        .args(["-s", "256", "-c", "png", "-q", "8"])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if a {
        return true;
    }
    let vf = format!(
        "thumbnail=50,scale={s}:{s}:force_original_aspect_ratio=decrease",
        s = STRIP_SIZE
    );
    Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(src)
        .args(["-vf", &vf, "-frames:v", "1"])
        .arg(tp)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// One-time rename of a cache built under the id-keyed scheme (`<id>.png`, `<id>.still`, `<id>.strip<n>-<ms>.png`)
/// to path keys, using the index to map each id to its path(s). Legacy files whose id is no longer in the index are
/// orphans and are removed. Idempotent: a marker file records that the directory is on the new scheme.
pub fn migrate_cache(c: &Cfg, rows: &[FileRow]) -> Result<usize, String> {
    let marker = c.thumbs.join(MIGRATED_MARKER);
    if marker.exists() {
        return Ok(0);
    }
    let Ok(rd) = std::fs::read_dir(&c.thumbs) else {
        return Ok(0);
    };
    let mut by_id: std::collections::HashMap<&str, Vec<String>> = Default::default();
    for r in rows {
        by_id
            .entry(r.id.as_str())
            .or_default()
            .push(library_cache_key(c, &r.path));
    }
    let (mut moved, mut dropped, mut legacy) = (0usize, 0usize, vec![]);
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Some((id, rest)) = name.split_once('.') else {
            continue;
        };
        if id.len() == 17
            && id.starts_with('p')
            && id[1..].chars().all(|ch| ch.is_ascii_hexdigit())
            && matches!(rest, "png" | "still" | "strip.png" | "strip.txt")
        {
            continue;
        } // already new
        legacy.push(e.path());
        let Some(keys) = by_id.get(id) else { continue };
        for key in keys {
            let target = match rest {
                "png" => Some((thumb_path(c, key), None)),
                "still" => Some((still_marker(c, key), None)),
                r if r.starts_with("strip") && r.ends_with(".png") => {
                    let spec = &r["strip".len()..r.len() - ".png".len()];
                    spec.split_once('-')
                        .and_then(|(n, ms)| Some((n.parse::<u32>().ok()?, ms.parse::<u64>().ok()?)))
                        .map(|(n, ms)| {
                            (
                                strip_png(c, key),
                                Some(format!("{n} {} {ms}\n", c.strip_frames)),
                            )
                        })
                }
                _ => None,
            };
            if let Some((new, meta)) = target {
                if new.exists() {
                    continue;
                }
                if std::fs::hard_link(e.path(), &new)
                    .or_else(|_| std::fs::copy(e.path(), &new).map(|_| ()))
                    .is_ok()
                {
                    if let Some(m) = meta {
                        let _ = std::fs::write(strip_meta(c, key), m);
                    }
                    moved += 1;
                }
            }
        }
    }
    for p in &legacy {
        if std::fs::remove_file(p).is_ok() {
            dropped += 1;
        }
    }
    std::fs::write(
        &marker,
        format!("cache keyed by path since {}\n", crate::writer::now_s()),
    )
    .map_err(|e| e.to_string())?;
    eprintln!(
        "thumbnail cache: {moved} files re-keyed by path, {} legacy files removed",
        dropped
    );
    Ok(moved)
}

/// Build the whole cache on `index_threads` workers (same configurable worker limit as reindex):
/// each worker takes every k-th row, makes its thumbnail and, for videos and animated images, its storyboard strip.
/// Prints progress every 1000 files so a 26k run on the server can be watched from a log.
pub fn build_thumbs(c: &Cfg, only: Option<&[&FileRow]>) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let all = db.all()?;
    migrate_cache(c, &all)?;
    let refs: Vec<&FileRow> = all.iter().collect();
    let rows = only.unwrap_or(&refs);
    let total = rows.len();
    let k = c.index_threads.max(1);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let (n, strips) = std::thread::scope(|sc| {
        let hs: Vec<_> = (0..k)
            .map(|w| {
                let done = &done;
                sc.spawn(move || {
                    let (mut n, mut strips) = (0usize, 0usize);
                    for r in rows.iter().skip(w).step_by(k) {
                        n += ensure_thumbs(c, &[*r]).len();
                        if (r.kind == "video"
                            || matches!(r.format.as_str(), "gif" | "webp" | "apng"))
                            && ensure_strip(c, r).is_some()
                        {
                            strips += 1;
                        }
                        let d = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        if d.is_multiple_of(1000) {
                            println!("  {d}/{total}");
                        }
                    }
                    (n, strips)
                })
            })
            .collect();
        hs.into_iter()
            .map(|h| h.join().unwrap_or((0, 0)))
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    println!("{n} thumbnails, {strips} animated strips (videos + animated GIF/WebP/APNG, up to {} frames) on {k} threads in {}", c.strip_frames, c.thumbs.display());
    Ok(())
}

pub fn grab(c: &Cfg, _db: &Db, q: &str, hits: &[&FileRow]) -> Result<(), String> {
    if std::env::var("MEMETAG_FEH").is_err() {
        return crate::launch_gui(c, q);
    }
    if hits.is_empty() {
        return Err(format!("nothing matches {q:?}"));
    }
    let thumbs = ensure_thumbs(c, hits);
    if thumbs.is_empty() {
        return Err("no thumbnails could be made".into());
    }
    let me = crate::self_exe().ok_or("memetag CLI is not installed")?;
    let mut cmd = Command::new("feh");
    cmd.args([
        "--thumbnails",
        "--thumb-width",
        "220",
        "--thumb-height",
        "220",
        "--limit-width",
        "1400",
        "--index-info",
        "",
        "--title",
        &format!(
            "memetag: {q}  ({} hits) — click = copy to clipboard",
            hits.len()
        ),
        "--action",
        &format!("{} clip %F", me.display()),
    ]);
    for (t, _) in &thumbs {
        cmd.arg(t);
    }
    eprintln!("{} thumbnails → feh (click one to copy it)", thumbs.len());
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("feh: {e}"))?;
    Ok(())
}

/// `arg` may be a thumbnail path (…/thumbs/<key>.png), an id prefix, or a real file path.
pub fn clip(c: &Cfg, arg: &str) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let p = Path::new(arg);
    let target: PathBuf = if p.starts_with(&c.thumbs) || (p.parent() == Some(c.thumbs.as_path())) {
        let key = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = key.split('.').next().unwrap_or("").to_string();
        let rel = db
            .all()?
            .into_iter()
            .find(|r| library_cache_key(c, &r.path) == key)
            .map(|r| r.path)
            .ok_or(format!("no file for cache key {key}"))?;
        c.file_path(&rel)?
    } else if p.exists() {
        p.to_path_buf()
    } else {
        c.file_path(&db.path_by_id(arg).ok_or(format!("no file or id {arg}"))?)?
    };
    let (name, mime) = clip_path(&target)?;
    println!("{}", target.display());
    let _ = (name, mime);
    Ok(())
}

/// A `file://` URI for a path: every byte outside the unreserved set and `/` is percent-encoded, as GLib and Qt read
/// them. Bare, a `#` ended the URI at the directory, a `?` cut the name and a `%` made it invalid; 22 files in the
/// collection carry one of those (review, 2026-09-22).
pub fn file_uri(p: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut s = String::from("file://");
    for &b in p.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                s.push(b as char)
            }
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

/// Put one file on the clipboard; returns (file name, targets offered). Slow (reads the original, may re-encode): call it off the UI thread.
pub fn clip_path(target: &Path) -> Result<(String, String), String> {
    use crate::clipboard::Entry;
    let bytes = std::fs::read(target).map_err(|e| format!("{}: {e}", target.display()))?;
    let kind = crate::containers::sniff(&bytes);
    let uri = || Entry {
        mime: "text/uri-list".into(),
        data: format!(
            "{}\r\n",
            file_uri(&target.canonicalize().unwrap_or(target.to_path_buf()))
        )
        .into_bytes(),
    };
    let png = |img: image::DynamicImage| -> Result<Entry, String> {
        let mut v = std::io::Cursor::new(Vec::new());
        img.write_to(&mut v, image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        Ok(Entry {
            mime: "image/png".into(),
            data: v.into_inner(),
        })
    };
    // PNG goes as it is. JPEG/WebP become PNG: browsers and chat apps accept image/png from the clipboard and mostly ignore image/jpeg.
    // A GIF goes as image/gif for browsers and Discord *and* as its file URL: Qt apps (Telegram Desktop, 64gram) take file
    // URLs before image data, and image data alone reaches them as a still first frame.
    let entries: Vec<Entry> = match kind {
        crate::containers::Kind::Png => vec![Entry {
            mime: "image/png".into(),
            data: bytes,
        }],
        crate::containers::Kind::Gif => vec![
            Entry {
                mime: "image/gif".into(),
                data: bytes,
            },
            uri(),
        ],
        crate::containers::Kind::Jpeg
        | crate::containers::Kind::WebP
        | crate::containers::Kind::Avif => vec![png(crate::containers::decode(&bytes)?)?],
        crate::containers::Kind::Other if crate::containers::decode(&bytes).is_ok() => {
            vec![png(crate::containers::decode(&bytes).unwrap())?]
        }
        _ => vec![uri()],
    };
    let mime = entries
        .iter()
        .map(|e| e.mime.as_str())
        .collect::<Vec<_>>()
        .join(" + ");
    crate::clipboard::own(&entries)?;
    let name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    for tool in ["fyi", "notify-send"] {
        if Command::new(tool)
            .args([
                "-a",
                "memetag",
                "-t",
                "3000",
                "Copied to clipboard",
                &format!("{name} ({mime})"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            break;
        }
    }
    Ok((name, mime))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: &str, path: &str, format: &str, kind: &str) -> FileRow {
        FileRow {
            id: id.into(),
            path: path.into(),
            format: format.into(),
            kind: kind.into(),
            width: 1,
            height: 1,
            size: 1,
            mtime: 1_600_000_000.0,
            created_at: 0.0,
            tagged_at: 0.0,
            xmp_tag_count: 0,
            tags: Default::default(),
            text: String::new(),
        }
    }
    #[test]
    fn cache_is_keyed_by_path_and_migrates_from_ids() {
        let root = std::env::temp_dir().join(format!("memetag-grab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let c = Cfg {
            sources: None,
            active_source: None,
            root: root.join("memes"),
            db: root.join("index.sqlite"),
            thumbs: root.join("thumbs"),
            vocab: Default::default(),
            preview_fps: 12.0,
            strip_frames: 24,
            index_threads: 1,
            texture_budget_mb: 64,
            embed_model: None,
            ocr_model: None,
            ocr_prompt: String::new(),
            translate_model: None,
            translate_prompt: String::new(),
            speech_command: String::new(),
            ollama_url: String::new(),
        };
        std::fs::create_dir_all(&c.thumbs).unwrap();
        let rows = vec![
            row("b0011223344556677", "Old/a.png", "png", "image"),
            row("aa11223344556677", "Reactions/b.gif", "gif", "image"),
            row("aa11223344556677", "Reactions/b copy.gif", "gif", "image"),
        ];
        std::fs::write(c.thumbs.join("b0011223344556677.png"), b"thumb-a").unwrap();
        std::fs::write(c.thumbs.join("aa11223344556677.png"), b"thumb-b").unwrap();
        std::fs::write(
            c.thumbs.join("aa11223344556677.strip12-1500.png"),
            b"strip-b",
        )
        .unwrap();
        std::fs::write(c.thumbs.join("deadbeefdeadbeef.png"), b"orphan").unwrap();
        assert_eq!(migrate_cache(&c, &rows).unwrap(), 5);
        assert_eq!(
            std::fs::read(thumb_path(&c, &cache_key("Old/a.png"))).unwrap(),
            b"thumb-a"
        );
        for p in ["Reactions/b.gif", "Reactions/b copy.gif"] {
            let key = cache_key(p);
            assert_eq!(std::fs::read(thumb_path(&c, &key)).unwrap(), b"thumb-b");
            assert_eq!(
                strip_cached(&c, &key, &rows[1]).unwrap(),
                (strip_png(&c, &key), 12, 1.5)
            );
        }
        let names: Vec<String> = std::fs::read_dir(&c.thumbs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| n.starts_with('p') || n == MIGRATED_MARKER),
            "{names:?}"
        );
        assert_eq!(
            migrate_cache(&c, &rows).unwrap(),
            0,
            "second run is a no-op"
        );
        // freshness comes from the indexed mtime, not from stat'ing the source (which does not even exist here)
        assert!(ensure_thumbs(&c, &[&rows[0]]).len() == 1);
        let mut stale = rows[0].clone();
        stale.mtime = 4_000_000_000.0;
        assert!(
            ensure_thumbs(&c, &[&stale]).is_empty(),
            "a thumbnail older than the indexed mtime is regenerated (and fails: no source)"
        );
        let mut other = rows[1].clone();
        let _ = &mut other;
        let mut c2 = c.clone();
        c2.strip_frames = 8;
        assert!(
            strip_cached(&c2, &cache_key("Reactions/b.gif"), &rows[1]).is_none(),
            "a strip made for another frame budget is not reused"
        );
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn file_uris_are_percent_encoded() {
        assert_eq!(
            file_uri(Path::new("/m/#1 meme%.gif")),
            "file:///m/%231%20meme%25.gif"
        );
        assert_eq!(
            file_uri(Path::new("/Old/why?.gif")),
            "file:///Old/why%3F.gif"
        );
        assert_eq!(
            file_uri(Path::new("/café/a-b_c.~1")),
            "file:///caf%C3%A9/a-b_c.~1"
        );
        assert_eq!(
            scratch(Path::new("/t/pabc.png")).to_str().unwrap(),
            format!("/t/pabc.tmp{}.png", std::process::id())
        );
        assert!(scratch(Path::new("/t/pabc.strip.png"))
            .to_str()
            .unwrap()
            .starts_with("/t/pabc.strip.tmp"));
    }
    #[test]
    fn packet_timing_counts_frames_and_ends_at_the_last_packet() {
        assert_eq!(
            packet_timing("0.000000,0.121000\n0.121000,0.272000\n0.393000,0.061000\n"),
            (3, 0.454)
        );
        assert_eq!(
            packet_timing("0.0,0.5\nN/A,N/A\n1.0,0.13\n\n"),
            (3, 1.13),
            "N/A fields count as zero, blank lines are not packets"
        );
        assert_eq!(packet_timing(""), (0, 0.0));
    }
    #[test]
    fn animated_webp_gets_a_strip_timed_by_its_packets() {
        if Command::new("ffmpeg").arg("-version").output().is_err()
            || Command::new("ffprobe").arg("-version").output().is_err()
        {
            eprintln!("skipped: ffmpeg/ffprobe not installed");
            return;
        }
        let root = std::env::temp_dir().join(format!("memetag-webp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let c = Cfg {
            sources: None,
            active_source: None,
            root: root.join("memes"),
            db: root.join("index.sqlite"),
            thumbs: root.join("thumbs"),
            vocab: Default::default(),
            preview_fps: 12.0,
            strip_frames: 24,
            index_threads: 1,
            texture_budget_mb: 64,
            embed_model: None,
            ocr_model: None,
            ocr_prompt: String::new(),
            translate_model: None,
            translate_prompt: String::new(),
            speech_command: String::new(),
            ollama_url: String::new(),
        };
        std::fs::create_dir_all(&c.thumbs).unwrap();
        std::fs::create_dir_all(&c.root).unwrap();
        // 5 frames at 4 fps = 1.25 s, as an animated WebP (no container duration), the same clip as a GIF, and as an APNG
        for (name, extra) in [
            ("a.webp", vec!["-c:v", "libwebp_anim", "-lossless", "1"]),
            ("a.gif", vec![]),
            ("a.png", vec!["-c:v", "apng", "-plays", "0"]),
        ] {
            let ok = Command::new("ffmpeg")
                .args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=32x32:rate=4",
                    "-frames:v",
                    "5",
                ])
                .args(&extra)
                .arg(c.root.join(name))
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                eprintln!("skipped: this ffmpeg cannot write {name}");
                return;
            }
        }
        let mut rows = vec![
            row("b1", "a.webp", "webp", "image"),
            row("b2", "a.gif", "gif", "image"),
            row("b3", "a.png", "apng", "image"),
        ];
        for r in rows.iter_mut() {
            r.mtime = 0.0;
        }
        for r in &rows {
            let (strip, n, dur) =
                ensure_strip(&c, r).unwrap_or_else(|| panic!("no strip for {}", r.path));
            assert_eq!(n, 5, "{}: native frame count", r.path);
            assert!((dur - 1.25).abs() < 0.02, "{}: duration {dur}", r.path);
            assert!(strip.exists());
            assert_eq!(
                strip_cached(&c, &cache_key(&r.path), r).unwrap(),
                (strip, 5, dur),
                "{}: second call is served from the cache",
                r.path
            );
        }
        // the indexer labels the APNG so the grid treats it as animated; a plain PNG keeps its label and gets no strip
        let bytes = std::fs::read(c.root.join("a.png")).unwrap();
        let md = std::fs::metadata(c.root.join("a.png")).unwrap();
        assert_eq!(
            crate::index::scan_bytes(
                &c.root,
                &c.root.join("a.png"),
                &bytes,
                &md,
                &Default::default()
            )
            .row
            .format,
            "apng"
        );
        assert!(
            ensure_strip(&c, &row("b4", "a.png", "png", "image")).is_none(),
            "a row still labelled png is a still"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
