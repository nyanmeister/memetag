//! Tag proposals: take the files that carry a tag, rank every file without it by likeness, offer the top of the list
//! for a look. Nothing is written until the offer is accepted.
//!
//! Likeness comes in three modes. `image`: the CLIP ViT-B/32 embedding of the thumbnail (ONNX through `ort`, on the
//! CPU), cosine to the mean of the chosen files' vectors. `text`: a TF-IDF vector over the OCR text in the index,
//! cosine to the mean the same way. `both`: the two cosines summed. Declined offers are remembered per tag and their
//! mean is subtracted, so the ranking sharpens with use. The mode last used for a tag is remembered too.
//!
//! Vectors live in `phash2` under their own `alg`, 512 little-endian f32 per row, so `pull` and `reindex` drop them
//! with the file exactly as they drop the perceptual hash. The model file is not shipped: `~/.local/share/memetag/
//! models/clip-vit-b32-vision.onnx` (Xenova's ONNX export of openai/clip-vit-base-patch32, 352 MB), or `embed_model`
//! in config.toml.
use crate::{
    index::{Db, FileRow},
    Cfg,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

pub const ALG: &str = "clip-vit-b32";
pub const DIM: usize = 512;
pub const SIDE: u32 = 224;
// openai/clip-vit-base-patch32 preprocessor_config.json
const MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
const STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Image,
    Text,
    Both,
}
impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Image, Mode::Text, Mode::Both];
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Image => "image",
            Mode::Text => "text",
            Mode::Both => "both",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.as_str() == s)
    }
    fn image(self) -> bool {
        self != Mode::Text
    }
    fn text(self) -> bool {
        self != Mode::Image
    }
}

pub fn model_path(c: &Cfg) -> PathBuf {
    c.embed_model.clone().unwrap_or_else(|| {
        c.db.parent()
            .unwrap_or(std::path::Path::new("."))
            .join("models/clip-vit-b32-vision.onnx")
    })
}

// ---- the image vectors -------------------------------------------------------------------------------------------

/// The thumbnail as CLIP sees it: shortest edge to 224 (bicubic), centre 224×224, RGB scaled and normalised, CHW.
pub fn preprocess(img: &image::DynamicImage) -> Vec<f32> {
    let (w, h) = (img.width().max(1), img.height().max(1));
    let s = SIDE as f64 / w.min(h) as f64;
    let (nw, nh) = (
        ((w as f64 * s).round() as u32).max(SIDE),
        ((h as f64 * s).round() as u32).max(SIDE),
    );
    let small = img
        .resize_exact(nw, nh, image::imageops::FilterType::CatmullRom)
        .to_rgb8();
    let (x0, y0) = ((nw - SIDE) / 2, (nh - SIDE) / 2);
    let n = (SIDE * SIDE) as usize;
    let mut out = vec![0f32; 3 * n];
    for y in 0..SIDE {
        for x in 0..SIDE {
            let p = small.get_pixel(x0 + x, y0 + y);
            let i = (y * SIDE + x) as usize;
            for c in 0..3 {
                out[c * n + i] = (p[c] as f32 / 255.0 - MEAN[c]) / STD[c];
            }
        }
    }
    out
}

pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}
fn from_bytes(b: &[u8]) -> Option<Vec<f32>> {
    if b.len() != DIM * 4 {
        return None;
    }
    Some(
        b.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}
pub fn normalise(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Every cached vector, by path.
pub fn cached(db: &Db) -> Result<HashMap<String, Vec<f32>>, String> {
    Ok(db
        .cached_blobs(ALG)?
        .into_iter()
        .filter_map(|(p, b)| from_bytes(&b).map(|v| (p, v)))
        .collect())
}

/// The cache with every image row in `rows` embedded: the missing ones are read now from the thumbnail (else the
/// original), preprocessed on `index_threads` workers and run through the model in batches, written as they come.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct InferenceRequest {
    #[serde(default)]
    pub sources: Option<crate::sources::Library>,
    pub root: PathBuf,
    pub db: PathBuf,
    pub thumbs: PathBuf,
    pub embed_model: Option<PathBuf>,
    pub index_threads: usize,
    pub rows: Vec<FileRow>,
    pub limit: usize,
    pub progress: bool,
}

pub fn ensure(
    c: &Cfg,
    db: &Db,
    rows: &[FileRow],
    limit: usize,
    progress: bool,
) -> Result<HashMap<String, Vec<f32>>, String> {
    let have = cached(db)?;
    let missing: Vec<FileRow> = rows
        .iter()
        .filter(|r| r.kind == "image" && !have.contains_key(&r.path))
        .take(limit)
        .cloned()
        .collect();
    if missing.is_empty() {
        return Ok(have);
    }
    let request = InferenceRequest {
        sources: c.library()?.cloned(),
        root: c.root.clone(),
        db: c.db.clone(),
        thumbs: c.thumbs.clone(),
        embed_model: c.embed_model.clone(),
        index_threads: c.index_threads,
        rows: missing,
        limit,
        progress,
    };
    use std::process::{Command, Stdio};
    let mut child = Command::new(crate::companion("memetag-infer")?)
        .arg("--request")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let sent = serde_json::to_writer(
        child.stdin.take().ok_or("inference stdin unavailable")?,
        &request,
    );
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    sent.map_err(|e| e.to_string())?;
    let report = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("inference: {report}"));
    }
    if progress {
        eprint!("{report}");
    }
    cached(db)
}

// ---- the text vectors --------------------------------------------------------------------------------------------

/// TF-IDF over the index's OCR text: one sparse unit vector per file that has any, terms as ids.
pub struct TextSpace {
    docs: HashMap<String, Vec<(u32, f32)>>,
    terms: usize,
}
fn tokens(s: &str) -> Vec<String> {
    s.split(|ch: char| !ch.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_lowercase())
        .collect()
}
impl TextSpace {
    pub fn build(rows: &[FileRow]) -> TextSpace {
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut df: Vec<u32> = vec![];
        let mut counted: Vec<(String, HashMap<u32, u32>)> = vec![];
        for r in rows {
            if r.text.trim().is_empty() {
                continue;
            }
            let mut tf: HashMap<u32, u32> = HashMap::new();
            for t in tokens(&r.text) {
                let n = ids.len() as u32;
                let id = *ids.entry(t).or_insert(n);
                if id as usize >= df.len() {
                    df.push(0);
                }
                *tf.entry(id).or_insert(0) += 1;
            }
            for id in tf.keys() {
                df[*id as usize] += 1;
            }
            counted.push((r.path.clone(), tf));
        }
        let n = counted.len().max(1) as f32;
        let docs = counted
            .into_iter()
            .map(|(path, tf)| {
                let mut v: Vec<(u32, f32)> = tf
                    .into_iter()
                    .map(|(id, c)| {
                        (
                            id,
                            (1.0 + (c as f32).ln()) * (n / df[id as usize] as f32).ln(),
                        )
                    })
                    .collect();
                let norm = v.iter().map(|(_, w)| w * w).sum::<f32>().sqrt();
                if norm > 0.0 {
                    v.iter_mut().for_each(|(_, w)| *w /= norm);
                }
                v.sort_unstable_by_key(|(id, _)| *id);
                (path, v)
            })
            .collect();
        TextSpace {
            docs,
            terms: ids.len(),
        }
    }
    pub fn has(&self, path: &str) -> bool {
        self.docs.contains_key(path)
    }
    /// The unit mean of these files' vectors (the ones with text), dense; None when none has text.
    fn prototype(&self, paths: &HashSet<String>) -> Option<Vec<f32>> {
        let mut proto = vec![0f32; self.terms];
        let mut n = 0;
        for p in paths {
            if let Some(v) = self.docs.get(p) {
                for (id, w) in v {
                    proto[*id as usize] += w;
                }
                n += 1;
            }
        }
        if n == 0 {
            return None;
        }
        normalise(&mut proto);
        Some(proto)
    }
    fn cos(&self, path: &str, proto: &[f32]) -> Option<f32> {
        self.docs
            .get(path)
            .map(|v| v.iter().map(|(id, w)| w * proto[*id as usize]).sum())
    }
}

// ---- the ranking -------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Proposal {
    pub path: String,
    pub score: f32,
}

/// How much the rejections' centroid pulls, against the positives' full cosine. Measured 2026-09-23 (six tags, a
/// third hidden, the hand-tagged pool, the first 10 or 30 wrong offers rejected as a reviewer would): the full cosine
/// (1.0) wrecked image mode at 10 rejections (comic 40 → 3 of 50, 4chan post 28 → 0, twitter post 25 → 4); a weight
/// scaled by rejections over positives was second; 0.5 was best or tied in every cell and beat having no rejections
/// (comic 48, 4chan post 33, twitter post 31, cat 26, stonetoss 35, donald trump 21 of 50 at 10 rejections).
const NEG_WEIGHT: f32 = 0.5;

/// The unit mean of these files' image vectors; None when none is cached.
///
/// One centroid per side, measured (2026-09-23, eight tags, a third hidden, the hand-tagged pool): the mean of the
/// three nearest references found fewer of the hidden files in the top 50 for six tags of eight (twitter post 14
/// against 25, cat 8 against 18, donald trump 7 against 17) and tied on the other two. A tag's blur is its signal.
fn image_prototype(vecs: &HashMap<String, Vec<f32>>, paths: &HashSet<String>) -> Option<Vec<f32>> {
    let mut proto = vec![0f32; DIM];
    let mut n = 0;
    for p in paths {
        if let Some(v) = vecs.get(p) {
            proto.iter_mut().zip(v).for_each(|(a, b)| *a += b);
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    normalise(&mut proto);
    Some(proto)
}

/// Every candidate scored and sorted, best first. `positives` define the tag, `rejects` pull away from it (their
/// centroid's cosine is subtracted at `NEG_WEIGHT`), `candidates` says which rows may be offered (the caller excludes the files that
/// carry the tag).
pub fn rank(
    rows: &[FileRow],
    vecs: &HashMap<String, Vec<f32>>,
    text: Option<&TextSpace>,
    positives: &HashSet<String>,
    rejects: &HashSet<String>,
    candidates: impl Fn(&FileRow) -> bool,
    mode: Mode,
) -> Result<Vec<Proposal>, String> {
    let img_pos = mode
        .image()
        .then(|| image_prototype(vecs, positives))
        .flatten();
    let img_neg = mode
        .image()
        .then(|| image_prototype(vecs, rejects))
        .flatten();
    let txt_pos = text
        .filter(|_| mode.text())
        .and_then(|t| t.prototype(positives));
    let txt_neg = text
        .filter(|_| mode.text())
        .and_then(|t| t.prototype(rejects));
    if img_pos.is_none() && txt_pos.is_none() {
        return Err(match mode {
            Mode::Image => {
                "none of the chosen files has an image vector yet (memetag embeddings)".into()
            }
            Mode::Text => "none of the chosen files has any text".into(),
            Mode::Both => "none of the chosen files has an image vector or any text".into(),
        });
    }
    let neg_w = NEG_WEIGHT;
    let mut out: Vec<Proposal> = rows
        .iter()
        .filter(|r| candidates(r))
        .filter_map(|r| {
            let mut score = 0f32;
            let mut any = false;
            if let (Some(p), Some(v)) = (&img_pos, vecs.get(&r.path)) {
                score += dot(v, p);
                if let Some(n) = &img_neg {
                    score -= neg_w * dot(v, n);
                }
                any = true;
            }
            if let (Some(p), Some(t)) = (&txt_pos, text) {
                if let Some(c) = t.cos(&r.path, p) {
                    score += c;
                    if let Some(n) = &txt_neg {
                        score -= neg_w * t.cos(&r.path, n).unwrap_or(0.0);
                    }
                    any = true;
                }
            }
            any.then(|| Proposal {
                path: r.path.clone(),
                score,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(out)
}

// ---- what the index remembers ------------------------------------------------------------------------------------

pub fn rejects(db: &Db, tag: &str) -> Result<HashSet<String>, String> {
    let mut st = db
        .conn
        .prepare("SELECT path FROM proposal_rejects WHERE tag=?1")
        .map_err(|e| e.to_string())?;
    let v = st
        .query_map([tag], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(v)
}
pub fn reject(db: &Db, tag: &str, paths: &[String]) -> Result<(), String> {
    let tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for p in paths {
        tx.execute(
            "INSERT OR IGNORE INTO proposal_rejects(tag, path) VALUES(?1, ?2)",
            [tag, p],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}
pub fn unreject(db: &Db, tag: &str, paths: &[String]) -> Result<(), String> {
    let tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
    for p in paths {
        tx.execute(
            "DELETE FROM proposal_rejects WHERE tag=?1 AND path=?2",
            [tag, p],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.commit().map_err(|e| e.to_string())
}
pub fn mode_for(db: &Db, tag: &str) -> Option<Mode> {
    db.conn
        .query_row("SELECT mode FROM proposal_modes WHERE tag=?1", [tag], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| Mode::parse(&s))
}
pub fn remember_mode(db: &Db, tag: &str, mode: Mode) -> Result<(), String> {
    db.conn
        .execute(
            "INSERT OR REPLACE INTO proposal_modes(tag, mode) VALUES(?1, ?2)",
            [tag, mode.as_str()],
        )
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ---- the commands ------------------------------------------------------------------------------------------------

/// `memetag embeddings [--limit N]`: fill the image vectors for every image in the index.
pub fn run_embeddings(c: &Cfg, args: &[String]) -> Result<(), String> {
    let mut limit = usize::MAX;
    let mut args = args.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--limit" => {
                limit = args
                    .next()
                    .ok_or("--limit needs a number")?
                    .parse()
                    .map_err(|_| "invalid limit")?
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let have = ensure(c, &db, &rows, limit, true)?;
    let images = rows.iter().filter(|r| r.kind == "image").count();
    println!(
        "{} of {images} images have a vector",
        have.len().min(images)
    );
    Ok(())
}

/// `memetag propose <tag> [--mode image|text|both] [--top N] [--measure [--seed N] [--pool tagged|all] [--rejects N]]`.
pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let (mut tag, mut mode, mut top, mut measure, mut seed, mut pool_all, mut n_rejects) = (
        None::<String>,
        None::<Mode>,
        50usize,
        false,
        1u64,
        false,
        0usize,
    );
    let mut args = args.iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--mode" => {
                let m = args.next().ok_or("--mode needs image, text or both")?;
                mode = Some(Mode::parse(m).ok_or_else(|| format!("unknown mode {m}"))?);
            }
            "--top" => {
                top = args
                    .next()
                    .ok_or("--top needs a number")?
                    .parse()
                    .map_err(|_| "invalid --top")?
            }
            "--measure" => measure = true,
            "--rejects" => {
                n_rejects = args
                    .next()
                    .ok_or("--rejects needs a number")?
                    .parse()
                    .map_err(|_| "invalid --rejects")?
            }
            "--pool" => {
                pool_all = match args.next().map(String::as_str) {
                    Some("all") => true,
                    Some("tagged") => false,
                    _ => return Err("--pool needs tagged or all".into()),
                }
            }
            "--seed" => {
                seed = args
                    .next()
                    .ok_or("--seed needs a number")?
                    .parse()
                    .map_err(|_| "invalid --seed")?
            }
            other if tag.is_none() && !other.starts_with("--") => tag = Some(other.to_string()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let tag = c.vocab.canon(&tag.ok_or("propose needs a tag")?);
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let vecs = cached(&db)?;
    if vecs.is_empty() {
        return Err("no image vectors yet: run memetag embeddings first".into());
    }
    let text = TextSpace::build(&rows);
    let with_tag: HashSet<String> = rows
        .iter()
        .filter(|r| r.tags.contains(&tag))
        .map(|r| r.path.clone())
        .collect();
    if with_tag.is_empty() {
        return Err(format!("no file carries the tag \"{tag}\""));
    }
    if measure {
        return run_measure(
            &rows, &vecs, &text, &tag, &with_tag, mode, top, seed, pool_all, n_rejects,
        );
    }
    let mode = mode.or_else(|| mode_for(&db, &tag)).unwrap_or(Mode::Image);
    let rejected = rejects(&db, &tag)?;
    let ranked = rank(
        &rows,
        &vecs,
        Some(&text),
        &with_tag,
        &rejected,
        |r| !with_tag.contains(&r.path) && !rejected.contains(&r.path),
        mode,
    )?;
    println!(
        "# {tag}: {} files carry it, {} rejected before, mode {}, {} candidates scored",
        with_tag.len(),
        rejected.len(),
        mode.as_str(),
        ranked.len()
    );
    for p in ranked.iter().take(top) {
        println!("{:.3}  {}", p.score, p.path);
    }
    Ok(())
}

/// Hide a third of the tag's files, define it by the rest, and see where the hidden ones land among every file
/// without the tag. Reports precision and recall at a few depths, the median rank, and what chance would give.
#[allow(clippy::too_many_arguments)]
fn run_measure(
    rows: &[FileRow],
    vecs: &HashMap<String, Vec<f32>>,
    text: &TextSpace,
    tag: &str,
    with_tag: &HashSet<String>,
    mode: Option<Mode>,
    top: usize,
    seed: u64,
    pool_all: bool,
    n_rejects: usize,
) -> Result<(), String> {
    let mut all: Vec<&String> = with_tag.iter().collect();
    all.sort();
    // a small deterministic shuffle (splitmix64), so two runs with one seed hide the same files
    let mut s = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut rnd = move || {
        s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..all.len()).rev() {
        let j = (rnd() % (i as u64 + 1)) as usize;
        all.swap(i, j);
    }
    let cut = (all.len() / 3).max(1);
    if all.len() < 3 {
        return Err(format!(
            "\"{tag}\" is on {} files; three or more are needed to hide a third",
            all.len()
        ));
    }
    let hidden: HashSet<String> = all[..cut].iter().map(|s| s.to_string()).collect();
    let shown: HashSet<String> = all[cut..].iter().map(|s| s.to_string()).collect();
    let usable = |m: Mode| {
        hidden
            .iter()
            .filter(|p| (m.image() && vecs.contains_key(*p)) || (m.text() && text.has(p)))
            .count()
    };
    // the pool is the hand-tagged files without the tag: there a missing tag was a decision, while among the
    // untagged ones a file at rank 1 may well deserve the tag and would count as a miss
    let candidates = |r: &FileRow| {
        !shown.contains(&r.path) && (hidden.contains(&r.path) || pool_all || r.xmp_tag_count > 0)
    };
    let modes: Vec<Mode> = mode.map(|m| vec![m]).unwrap_or_else(|| Mode::ALL.to_vec());
    let pool = rows.iter().filter(|r| candidates(r)).count() - hidden.len();
    println!(
        "# {tag}: {} files carry it; {} define it, {} hidden among {pool} {} files without it (seed {seed})",
        all.len(),
        shown.len(),
        hidden.len(),
        if pool_all { "" } else { "hand-tagged" }
    );
    println!(
        "{:<6} {:>7} {:>9} {:>9} {:>9} {:>9} {:>12}",
        "mode", "scored", "found@50", "@100", "@500", "@top", "median rank"
    );
    for m in modes {
        let mut ranked = rank(
            rows,
            vecs,
            Some(text),
            &shown,
            &HashSet::new(),
            candidates,
            m,
        )?;
        let mut rejected: HashSet<String> = HashSet::new();
        if n_rejects > 0 {
            // what a reviewer would reject: the first offers that are not the tag, top of the list down
            rejected = ranked
                .iter()
                .filter(|p| !hidden.contains(&p.path))
                .take(n_rejects)
                .map(|p| p.path.clone())
                .collect();
            let again = |r: &FileRow| candidates(r) && !rejected.contains(&r.path);
            ranked = rank(rows, vecs, Some(text), &shown, &rejected, again, m)?;
        }
        let ranks: Vec<usize> = ranked
            .iter()
            .enumerate()
            .filter(|(_, p)| hidden.contains(&p.path))
            .map(|(i, _)| i + 1)
            .collect();
        let at = |k: usize| ranks.iter().filter(|r| **r <= k).count();
        let median = if ranks.is_empty() {
            "-".to_string()
        } else {
            let mut r = ranks.clone();
            r.sort_unstable();
            r[r.len() / 2].to_string()
        };
        println!(
            "{:<6} {:>7} {:>9} {:>9} {:>9} {:>9} {:>12}",
            if rejected.is_empty() {
                m.as_str().to_string()
            } else {
                format!("{}-{}", m.as_str(), rejected.len())
            },
            ranked.len(),
            format!("{}/{}", at(50), usable(m)),
            at(100),
            at(500),
            format!("{}@{top}", at(top)),
            median
        );
        let chance = 50.0 * hidden.len() as f64 / ranked.len().max(1) as f64;
        if m == Mode::Both {
            println!("# chance would put {chance:.1} of the hidden in the top 50");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(path: &str, text: &str) -> FileRow {
        FileRow {
            id: path.into(),
            path: path.into(),
            format: "png".into(),
            kind: "image".into(),
            width: 1,
            height: 1,
            size: 1,
            mtime: 0.0,
            created_at: 0.0,
            tagged_at: 0.0,
            xmp_tag_count: 0,
            tags: Default::default(),
            text: text.into(),
        }
    }
    #[test]
    fn preprocess_is_the_clip_shape_and_centred() {
        let img = image::DynamicImage::new_rgb8(300, 200);
        let v = preprocess(&img);
        assert_eq!(v.len(), 3 * 224 * 224);
        // a black image is (0 - mean) / std in every channel
        assert!((v[0] - (-MEAN[0] / STD[0])).abs() < 1e-5);
        assert!((v[2 * 224 * 224] - (-MEAN[2] / STD[2])).abs() < 1e-5);
        let tiny = image::DynamicImage::new_rgb8(10, 300);
        assert_eq!(preprocess(&tiny).len(), 3 * 224 * 224);
    }
    #[test]
    fn bytes_round_trip_and_reject_wrong_length() {
        let v: Vec<f32> = (0..DIM).map(|i| i as f32 * 0.5).collect();
        assert_eq!(from_bytes(&to_bytes(&v)).unwrap(), v);
        assert!(from_bytes(&[0u8; 7]).is_none());
    }
    #[test]
    fn image_ranking_puts_the_lookalike_first_and_rejects_pull_down() {
        let mut vecs = HashMap::new();
        let unit = |i: usize| {
            let mut v = vec![0f32; DIM];
            v[i] = 1.0;
            v
        };
        vecs.insert("a".to_string(), unit(0));
        vecs.insert("b".to_string(), unit(0));
        vecs.insert("near".to_string(), {
            let mut v = unit(0);
            v[1] = 0.5;
            normalise(&mut v);
            v
        });
        vecs.insert("far".to_string(), unit(5));
        let rows: Vec<FileRow> = ["a", "b", "near", "far"]
            .iter()
            .map(|p| row(p, ""))
            .collect();
        let pos: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let r = rank(
            &rows,
            &vecs,
            None,
            &pos,
            &HashSet::new(),
            |r| !pos.contains(&r.path),
            Mode::Image,
        )
        .unwrap();
        assert_eq!(r[0].path, "near");
        assert!(r[0].score > r[1].score);
        let near_before = r[0].score;
        let rej: HashSet<String> = ["near"].iter().map(|s| s.to_string()).collect();
        let r = rank(
            &rows,
            &vecs,
            None,
            &pos,
            &rej,
            |r| !pos.contains(&r.path),
            Mode::Image,
        )
        .unwrap();
        // "near" is its own rejection, so it loses NEG_WEIGHT of a full cosine; "far" is orthogonal and loses nothing
        let near = r.iter().find(|p| p.path == "near").unwrap();
        assert!(
            (near_before - near.score - NEG_WEIGHT).abs() < 1e-5,
            "a rejected lookalike drags its neighbours down by half their likeness: {near_before} -> {}",
            near.score
        );
        let far = r.iter().find(|p| p.path == "far").unwrap();
        assert!((far.score - 0.0).abs() < 1e-5);
        assert!(rank(&rows, &vecs, None, &pos, &rej, |_| true, Mode::Text).is_err());
    }
    #[test]
    fn text_ranking_matches_shared_words() {
        let rows = vec![
            row("a", "the cat sat on the mat"),
            row("b", "a cat on a mat"),
            row("c", "cat mat cat"),
            row("d", "quarterly earnings report"),
            row("e", ""),
        ];
        let t = TextSpace::build(&rows);
        assert!(t.has("a") && !t.has("e"));
        let pos: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let r = rank(
            &rows,
            &HashMap::new(),
            Some(&t),
            &pos,
            &HashSet::new(),
            |r| !pos.contains(&r.path),
            Mode::Text,
        )
        .unwrap();
        assert_eq!(
            r.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["c", "d"],
            "e has no text and is not scored"
        );
        assert!(r[0].score > r[1].score);
    }
    #[test]
    fn modes_parse_and_print() {
        for m in Mode::ALL {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse("x"), None);
    }
}
