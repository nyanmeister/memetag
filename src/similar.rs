//! Perceptual hashes: `similar:<image>` in a search, `memetag similar <file>` on the command line, `memetag dupes` groups.
//!
//! Every image is hashed as the grid sees it: scaled to fit 256×256 (`image::thumbnail`; the cached thumbnail is exactly
//! that, saved losslessly), then pHash — DCT of a 32×32 grey image, its 16×16 low frequencies, each bit "above the
//! median coefficient", 256 bits. Library hashes come from the local thumbnail cache, so filling the cache never reads
//! the mount; a file without a fresh thumbnail is read from the root and downscaled the same way. The query image takes
//! the same path, so a copy of a library file lands at distance 0.
//!
//! Measured 2026-09-19 on the real index (26,481 images): with the mean as the bit threshold (the first version, alg
//! `dct16t`), flat images — white screenshots of tweets and chat logs — got sparse hashes (median 14 bits set against 61
//! for the library) and 1,915 of them chained into one `dupes` group; three "pairs" checked by eye were unrelated. With
//! the median every hash has 128 bits set, the same chain has 79 close pairs left and all are real copies, 619 copies
//! with detail sit at 2 bits (median), 8 (90 %), 12 (95 %), and random pairs never come under 106. Hence the default
//! of 20 bits and no detail heuristic. Hashing all thumbnails takes 7 s on four threads.
//!
//! Hashes are cached in the index (`phash2`, alg `dct16m`) keyed by path; `index::write_row` drops the row when a
//! file's id (its pixel hash) changes, so a replaced file is hashed again. The cache is filled on first use: the grid
//! does it on a thread with a toast, the commands on the spot with progress on stderr; `pull` keeps it complete once
//! it exists. `groups` (the Duplicates card, `memetag dupes`) is complete linkage: every member of a group is within the
//! distance of every other, so look-alikes never chain two files that do not resemble each other.
use crate::index::{Db, FileRow};
use crate::Cfg;
use image_hasher::{HashAlg, HasherConfig};
use rusqlite::params;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};

pub const ALG: &str = "dct16m";
pub const DEFAULT_DISTANCE: u32 = 20;
/// `similar:clipboard` hashes the image on the clipboard (the camera button in the grid writes this).
pub const CLIPBOARD: &str = "clipboard";

fn hasher() -> image_hasher::Hasher {
    HasherConfig::new()
        .hash_size(16, 16)
        .hash_alg(HashAlg::Median)
        .preproc_dct()
        .to_hasher()
}
/// Hash an image the way the library is hashed: downscaled to fit 256×256 first (a no-op for a cached thumbnail).
pub fn hash_image(img: &image::DynamicImage) -> Vec<u8> {
    let h = hasher();
    if img.width() > 256 || img.height() > 256 {
        h.hash_image(&img.thumbnail(256, 256))
    } else {
        h.hash_image(img)
    }
    .as_bytes()
    .to_vec()
}
/// Hash a file's bytes (any container the index accepts; animated ones hash their first frame).
pub fn hash_bytes(b: &[u8]) -> Result<Vec<u8>, String> {
    crate::containers::decode(b).map(|img| hash_image(&img))
}
/// A median hash has exactly half its bits set; more means coefficients tied at the median, i.e. a flat image (a solid
/// colour, a 1×1 pixel, a nearly blank page). Such a hash carries no detail to match on, so it matches nothing: 6 of
/// the 26,481 library hashes on 2026-09-19, and without this rule they formed one group with a mostly white poster.
pub fn informative(h: &[u8]) -> bool {
    h.iter().map(|b| b.count_ones()).sum::<u32>() <= 144
}
pub fn distance(a: &[u8], b: &[u8]) -> u32 {
    if a.len() != b.len() {
        return u32::MAX;
    }
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

/// Every cached hash, by path.
pub fn cached(db: &Db) -> Result<HashMap<String, Vec<u8>>, String> {
    let mut st = db
        .conn
        .prepare("SELECT path, hash FROM phash2 WHERE alg=?1")
        .map_err(|e| e.to_string())?;
    let v = st
        .query_map([ALG], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(v)
}
/// Whether anyone has asked for image search on this index yet (pull keeps the cache complete only then).
pub fn in_use(db: &Db) -> bool {
    db.conn
        .query_row("SELECT count(*) FROM phash2 WHERE alg=?1", [ALG], |r| {
            r.get::<_, i64>(0)
        })
        .map(|n| n > 0)
        .unwrap_or(false)
}

/// The cache with every image row in `rows` hashed: the missing ones are done now, from the thumbnail when it is
/// fresh, else from the original, on `index_threads` workers, and written to the index as they come in.
/// `progress` prints a line per 2,000 files to stderr (the commands); the grid passes false.
pub fn ensure(
    c: &Cfg,
    db: &Db,
    rows: &[FileRow],
    progress: bool,
) -> Result<HashMap<String, Vec<u8>>, String> {
    let mut have = cached(db)?;
    let todo: Vec<FileRow> = rows
        .iter()
        .filter(|r| r.kind == "image" && !have.contains_key(&r.path))
        .cloned()
        .collect();
    if todo.is_empty() {
        return Ok(have);
    }
    if progress {
        eprintln!(
            "similar: hashing {} images not in the cache yet",
            todo.len()
        );
    }
    let t0 = std::time::Instant::now();
    let todo = Arc::new(todo);
    let next = Arc::new(AtomicUsize::new(0));
    let cfg = Arc::new(c.clone());
    let (tx, rx) = mpsc::sync_channel::<(String, Option<Vec<u8>>)>(c.index_threads * 2);
    let mut handles = vec![];
    for _ in 0..c.index_threads.max(1) {
        let (todo, next, tx, cfg) = (todo.clone(), next.clone(), tx.clone(), cfg.clone());
        handles.push(std::thread::spawn(move || loop {
            let k = next.fetch_add(1, Ordering::Relaxed);
            if k >= todo.len() {
                break;
            }
            let r = &todo[k];
            let src = crate::grab::fresh_thumb(&cfg, r).or_else(|| cfg.file_path(&r.path).ok());
            let h = src
                .ok_or_else(|| "Invalid source path".to_string())
                .and_then(|p| std::fs::read(p).map_err(|e| e.to_string()))
                .map_err(|e| e.to_string())
                .and_then(|b| hash_bytes(&b))
                .map_err(|e| eprintln!("  cannot hash {}: {e}", r.path))
                .ok();
            if tx.send((r.path.clone(), h)).is_err() {
                break;
            }
        }));
    }
    drop(tx);
    let mut n = 0usize;
    let mut failed = 0usize;
    // one commit per 1,000 hashes: a commit per row made the first fill nine times slower than the hashing itself
    let mut tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for (path, h) in rx {
        n += 1;
        match h {
            Some(h) => {
                tx.execute(
                    "INSERT OR REPLACE INTO phash2 VALUES(?1, ?2, ?3)",
                    params![path, ALG, h],
                )
                .map_err(|e| e.to_string())?;
                have.insert(path, h);
            }
            None => failed += 1,
        }
        if n % 1000 == 0 {
            tx.commit().map_err(|e| e.to_string())?;
            tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        }
        if progress && n % 2000 == 0 {
            eprintln!("  {n}/{}", todo.len());
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    for h in handles {
        let _ = h.join();
    }
    if progress {
        eprintln!(
            "similar: hashed {} images ({failed} failed) in {:.1}s",
            n - failed,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(have)
}

/// Complete-linkage groups over `hashes`, closest pairs first: a file joins a group only when it is within `max` bits
/// of every member, and two groups merge only when all their cross pairs are. Groups of indices, biggest first.
/// The pair scan is the whole cost (n² / 2 four-word popcounts; 26k images take about a second).
pub fn groups(hashes: &[Vec<u8>], max: u32) -> Vec<Vec<usize>> {
    let words: Vec<[u64; 4]> = hashes
        .iter()
        .map(|h| if informative(h) { h.clone() } else { vec![] })
        .map(|h| {
            let mut w = [0u64; 4];
            for (k, c) in h.chunks(8).take(4).enumerate() {
                let mut b = [0u8; 8];
                b[..c.len()].copy_from_slice(c);
                w[k] = u64::from_le_bytes(b);
            }
            w
        })
        .collect();
    let dist = |i: usize, j: usize| -> u32 {
        let (a, b) = (&words[i], &words[j]);
        (0..4).map(|k| (a[k] ^ b[k]).count_ones()).sum()
    };
    let n = words.len();
    let mut pairs: Vec<(u32, usize, usize)> = vec![];
    let flat: Vec<bool> = hashes.iter().map(|h| !informative(h)).collect();
    for i in 0..n {
        if flat[i] {
            continue;
        }
        for j in i + 1..n {
            if flat[j] {
                continue;
            }
            let d = dist(i, j);
            if d <= max {
                pairs.push((d, i, j));
            }
        }
    }
    pairs.sort_unstable();
    let mut of: Vec<Option<usize>> = vec![None; n];
    let mut groups: Vec<Vec<usize>> = vec![];
    for (_, i, j) in pairs {
        match (of[i], of[j]) {
            (None, None) => {
                of[i] = Some(groups.len());
                of[j] = Some(groups.len());
                groups.push(vec![i, j]);
            }
            (Some(g), None) | (None, Some(g)) => {
                let k = if of[i].is_some() { j } else { i };
                if groups[g].iter().all(|&m| dist(m, k) <= max) {
                    groups[g].push(k);
                    of[k] = Some(g);
                }
            }
            (Some(a), Some(b)) if a != b => {
                if groups[a]
                    .iter()
                    .all(|&x| groups[b].iter().all(|&y| dist(x, y) <= max))
                {
                    let moved = std::mem::take(&mut groups[b]);
                    for &m in &moved {
                        of[m] = Some(a);
                    }
                    groups[a].extend(moved);
                }
            }
            _ => {}
        }
    }
    groups.retain(|g| g.len() > 1);
    groups.sort_by_key(|g| std::cmp::Reverse(g.len()));
    groups
}

/// The `similar:` resolver for one query: the library's hashes plus the hash of every image the query names.
/// A value is a library path first (its cached hash, no file read), else a file on this machine (`~` expands).
pub struct Lookup {
    library: HashMap<String, Vec<u8>>,
    queries: HashMap<String, Result<Vec<u8>, String>>,
}
impl Lookup {
    pub fn new(library: HashMap<String, Vec<u8>>) -> Lookup {
        Lookup {
            library,
            queries: HashMap::new(),
        }
    }
    /// Resolve the values a query names (from `query::similar_terms`); returns the problems, one line each.
    pub fn prepare(&mut self, values: &[String]) -> Vec<String> {
        let mut errors = vec![];
        for v in values {
            if self.queries.contains_key(v) {
                continue;
            }
            let r = match self.library.get(v) {
                Some(h) => Ok(h.clone()),
                None if v == CLIPBOARD => crate::clipboard::read_image()
                    .and_then(|b| hash_bytes(&b))
                    .map_err(|e| format!("similar: no image on the clipboard ({e})")),
                None => {
                    let home = std::env::var("HOME").unwrap_or_default();
                    let p = v
                        .strip_prefix("~/")
                        .map(|rest| std::path::PathBuf::from(&home).join(rest))
                        .unwrap_or_else(|| std::path::PathBuf::from(v));
                    std::fs::read(&p)
                        .map_err(|e| format!("similar: cannot read {v}: {e}"))
                        .and_then(|b| {
                            hash_bytes(&b).map_err(|e| format!("similar: {v} is not an image: {e}"))
                        })
                }
            };
            let r = r.and_then(|h| if informative(&h) { Ok(h) } else { Err(format!("similar: {v} is flat (a solid colour or a blank page), nothing to match on")) });
            if let Err(e) = &r {
                errors.push(e.clone());
            }
            self.queries.insert(v.clone(), r);
        }
        errors
    }
    /// Drop a resolved value so the next `prepare` reads it again (the clipboard changed).
    pub fn forget(&mut self, value: &str) {
        self.queries.remove(value);
    }
    pub fn hash(&self, path: &str) -> Option<&Vec<u8>> {
        self.library.get(path)
    }
    /// Hash distance between a query value and a library file; None when either side has no hash.
    pub fn distance(&self, value: &str, path: &str) -> Option<u32> {
        let q = self.queries.get(value)?.as_ref().ok()?;
        let h = self.library.get(path)?;
        if !informative(q) || !informative(h) {
            return None;
        }
        Some(distance(q, h))
    }
}

/// `memetag similar <file> [--distance N] [--limit N] [--grab]`: the library files that look like `file`.
pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let (mut file, mut max, mut limit, mut grab) =
        (None::<String>, DEFAULT_DISTANCE, 10usize, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let number = |v: Option<&String>| {
            v.and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| format!("similar: {a} needs a number"))
        };
        match a.as_str() {
            "--distance" => max = number(it.next())?,
            "--limit" => limit = number(it.next())? as usize,
            "--grab" => grab = true,
            x if x.starts_with("--") => return Err(format!("similar: unknown option {x}")),
            x => file = Some(x.to_string()),
        }
    }
    let file = &file.ok_or("similar: which file?")?;
    if grab {
        // fill the cache here, with progress, rather than behind the grid's toast
        let db = Db::open_cfg(&c)?;
        ensure(c, &db, &db.all()?, true)?;
        drop(db);
        let spec = if max == DEFAULT_DISTANCE {
            crate::query::quote_tag(file)
        } else {
            format!("{}@{max}", crate::query::quote_tag(file))
        };
        return crate::launch_gui(c, &format!("similar:{spec}"));
    }
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let mut lookup = Lookup::new(ensure(c, &db, &rows, true)?);
    if let Some(e) = lookup
        .prepare(std::slice::from_ref(file))
        .into_iter()
        .next()
    {
        return Err(e);
    }
    let mut scored: Vec<(u32, &FileRow)> = rows
        .iter()
        .filter_map(|r| lookup.distance(file, &r.path).map(|d| (d, r)))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
    let within = scored.iter().take_while(|(d, _)| *d <= max).count();
    for (d, r) in scored.iter().take(within.max(1).min(limit)) {
        println!(
            "{d:3}  {}  {}x{}  {} tags  {}",
            c.file_path(&r.path)?.display(),
            r.width,
            r.height,
            r.xmp_tag_count,
            r.format
        );
    }
    match scored.get(within) {
        Some((d, r)) if within > 0 => eprintln!("similar: {within} of {} images within {max} bits of {file}; next closest is {d} bits away: {}", scored.len(), r.path),
        Some(_) => eprintln!("similar: nothing within {max} bits of {file} among {} images; the closest is listed above", scored.len()),
        None => eprintln!("similar: no hashed images to compare against"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn picture(w: u32, h: u32) -> image::DynamicImage {
        image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let disc = ((fx - 0.6).powi(2) + (fy - 0.4).powi(2)).sqrt() < 0.2;
            let stripes = ((fx * 7.0) as u32 + (fy * 3.0) as u32) % 2 == 0;
            if disc {
                image::Rgb([240, 40, 40])
            } else if stripes {
                image::Rgb([(fx * 255.0) as u8, 200, 60])
            } else {
                image::Rgb([20, 30, (fy * 255.0) as u8])
            }
        }))
    }
    #[test]
    fn resize_and_reencode_stay_close_and_thumbnail_matches_exactly() {
        let big = picture(1000, 800);
        let h_big = hash_image(&big);
        assert_eq!(h_big.len() * 8, 256);
        // the cached thumbnail is thumbnail(256, 256) saved losslessly: identical hash
        let thumb = big.thumbnail(256, 256);
        let mut png = std::io::Cursor::new(vec![]);
        thumb.write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert_eq!(distance(&h_big, &hash_bytes(png.get_ref()).unwrap()), 0);
        // a resize on another path, and a JPEG re-encode, stay well within the default distance
        let resized = big.resize_exact(640, 512, image::imageops::FilterType::Triangle);
        assert!(distance(&h_big, &hash_image(&resized)) <= DEFAULT_DISTANCE);
        let mut jpg = std::io::Cursor::new(vec![]);
        big.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut jpg, 60,
        ))
        .unwrap();
        assert!(distance(&h_big, &hash_bytes(jpg.get_ref()).unwrap()) <= DEFAULT_DISTANCE);
        // an unrelated picture is far away
        let other = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(500, 500, |x, y| {
            image::Rgb([(x % 40) as u8 * 6, (y % 25) as u8 * 10, 128])
        }));
        assert!(distance(&h_big, &hash_image(&other)) > 40);
    }
    #[test]
    fn flat_images_match_nothing() {
        let flat = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            300,
            300,
            image::Rgb([250, 250, 250]),
        ));
        let h = hash_image(&flat);
        assert!(
            !informative(&h),
            "{} bits set",
            h.iter().map(|b| b.count_ones()).sum::<u32>()
        );
        assert!(informative(&hash_image(&picture(400, 300))));
        assert!(groups(&[h.clone(), h.clone(), vec![0xFF; 32]], 20).is_empty());
        let mut l = Lookup::new(HashMap::from([("flat.png".to_string(), h)]));
        assert_eq!(l.prepare(&["flat.png".into()]).len(), 1);
    }
    #[test]
    fn groups_are_complete_linkage() {
        // a: 0 bits; b: 10 bits set; c: 10 other bits set — a~b and a~c within 12, but b and c are 20 apart: no chain
        let (mut b, mut c) = (vec![0u8; 32], vec![0u8; 32]);
        b[0] = 0xFF;
        b[1] = 0x03;
        c[5] = 0xFF;
        c[6] = 0x03;
        let a = vec![0u8; 32];
        let d = {
            let mut d = b.clone();
            d[2] = 0x01;
            d
        }; // d is 1 from b, 11 from a, 21 from c
        let g = groups(
            &[a.clone(), b.clone(), c.clone(), d.clone(), vec![0xFF; 32]],
            12,
        );
        assert_eq!(g, vec![vec![1, 3, 0]]); // b,d first (distance 1), then a joins (within 12 of both); c is left out, the far one too
        assert!(groups(&[a, c], 12).len() == 1);
    }
    #[test]
    fn lookup_reads_library_paths_before_files() {
        // two informative hashes (128 bits each) with nothing in common
        let (mut a, mut b) = (vec![0u8; 32], vec![0u8; 32]);
        a[..16].fill(0xFF);
        b[16..].fill(0xFF);
        let mut lib = HashMap::new();
        lib.insert("Old/a.png".to_string(), a);
        lib.insert("Old/b.png".to_string(), b);
        let mut l = Lookup::new(lib);
        assert!(l.prepare(&["Old/a.png".into()]).is_empty());
        assert_eq!(l.distance("Old/a.png", "Old/a.png"), Some(0));
        assert_eq!(l.distance("Old/a.png", "Old/b.png"), Some(256));
        assert_eq!(l.distance("Old/a.png", "Old/none.png"), None);
        let errs = l.prepare(&["/nonexistent/x.png".into()]);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("cannot read"));
        assert_eq!(l.distance("/nonexistent/x.png", "Old/a.png"), None);
    }
}
