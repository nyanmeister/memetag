use memetag::{
    index::{Db, FileRow},
    propose::{cached, model_path, normalise, preprocess, to_bytes, ALG, DIM, SIDE},
    Cfg,
};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
};
const BATCH: usize = 32;
/// The model, loaded once per command; `embed` takes a batch of preprocessed images and returns unit vectors.
pub struct Model {
    session: ort::session::Session,
    input: String,
    output: String,
}
impl Model {
    pub fn load(c: &Cfg) -> Result<Model, String> {
        let p = model_path(c);
        if !p.is_file() {
            return Err(format!(
                "no embedding model at {}: download Xenova/clip-vit-base-patch32's onnx/vision_model.onnx there, or set embed_model in config.toml",
                p.display()
            ));
        }
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let load = |e: &dyn std::fmt::Display| format!("loading {}: {e}", p.display());
        let builder = ort::session::Session::builder().map_err(|e| load(&e))?;
        let mut builder = builder.with_intra_threads(threads).map_err(|e| load(&e))?;
        let session = builder.commit_from_file(&p).map_err(|e| load(&e))?;
        let input = session
            .inputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or("the model has no input")?;
        let names: Vec<String> = session
            .outputs()
            .iter()
            .map(|o| o.name().to_string())
            .collect();
        let output = names
            .iter()
            .find(|n| n.as_str() == "image_embeds")
            .or(names.first())
            .cloned()
            .ok_or("the model has no output")?;
        Ok(Model {
            session,
            input,
            output,
        })
    }
    pub fn embed(&mut self, imgs: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, String> {
        let n = imgs.len();
        if n == 0 {
            return Ok(vec![]);
        }
        let per = 3 * (SIDE * SIDE) as usize;
        let mut flat = Vec::with_capacity(n * per);
        for i in imgs {
            flat.extend_from_slice(i);
        }
        let t = ort::value::Tensor::from_array(([n, 3, SIDE as usize, SIDE as usize], flat))
            .map_err(|e| e.to_string())?;
        let out = self
            .session
            .run(ort::inputs![self.input.as_str() => t])
            .map_err(|e| format!("model run: {e}"))?;
        let (_, data) = out[self.output.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("model output: {e}"))?;
        if data.len() != n * DIM {
            return Err(format!(
                "model output {} has {} values for {n} images, expected {}",
                self.output,
                data.len(),
                n * DIM
            ));
        }
        Ok(data
            .chunks_exact(DIM)
            .map(|c| {
                let mut v = c.to_vec();
                normalise(&mut v);
                v
            })
            .collect())
    }
}

pub fn ensure(
    c: &Cfg,
    db: &Db,
    rows: &[FileRow],
    limit: usize,
    progress: bool,
) -> Result<HashMap<String, Vec<f32>>, String> {
    let mut have = cached(db)?;
    let todo: Vec<FileRow> = rows
        .iter()
        .filter(|r| r.kind == "image" && !have.contains_key(&r.path))
        .take(limit)
        .cloned()
        .collect();
    if todo.is_empty() {
        return Ok(have);
    }
    let mut model = Model::load(c)?;
    if progress {
        eprintln!("embeddings: {} images not in the cache yet", todo.len());
    }
    let t0 = std::time::Instant::now();
    let todo = Arc::new(todo);
    let next = Arc::new(AtomicUsize::new(0));
    let cfg = Arc::new(c.clone());
    let (tx, rx) = mpsc::sync_channel::<(String, Option<Vec<f32>>)>(BATCH * 2);
    let mut handles = vec![];
    for _ in 0..c.index_threads.max(1) {
        let (todo, next, tx, cfg) = (todo.clone(), next.clone(), tx.clone(), cfg.clone());
        handles.push(std::thread::spawn(move || loop {
            let k = next.fetch_add(1, Ordering::Relaxed);
            if k >= todo.len() {
                break;
            }
            let r = &todo[k];
            let src = memetag::grab::fresh_thumb(&cfg, r).or_else(|| cfg.file_path(&r.path).ok());
            let v = src
                .ok_or_else(|| "Invalid source path".to_string())
                .and_then(|p| std::fs::read(p).map_err(|e| e.to_string()))
                .ok()
                .and_then(|b| image::load_from_memory(&b).ok())
                .map(|img| preprocess(&img));
            if tx.send((r.path.clone(), v)).is_err() {
                break;
            }
        }));
    }
    drop(tx);
    let (mut done, mut skipped) = (0usize, 0usize);
    let mut batch: Vec<(String, Vec<f32>)> = Vec::with_capacity(BATCH);
    let mut flush = |batch: &mut Vec<(String, Vec<f32>)>,
                     have: &mut HashMap<String, Vec<f32>>|
     -> Result<(), String> {
        if batch.is_empty() {
            return Ok(());
        }
        let imgs: Vec<Vec<f32>> = batch.iter().map(|(_, v)| v.clone()).collect();
        let vecs = model.embed(&imgs)?;
        c.ensure_current()?;
        let tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        for ((path, _), v) in batch.drain(..).zip(vecs) {
            tx.execute(
                "INSERT OR REPLACE INTO phash2(path, alg, hash) VALUES(?1, ?2, ?3)",
                rusqlite::params![path, ALG, to_bytes(&v)],
            )
            .map_err(|e| e.to_string())?;
            have.insert(path, v);
        }
        tx.commit().map_err(|e| e.to_string())
    };
    for (path, v) in rx {
        match v {
            Some(v) => batch.push((path, v)),
            None => skipped += 1,
        }
        if batch.len() >= BATCH {
            flush(&mut batch, &mut have)?;
        }
        done += 1;
        if progress && done % 1000 == 0 {
            eprintln!(
                "embeddings: {done}/{} ({:.0}/s)",
                todo.len(),
                done as f64 / t0.elapsed().as_secs_f64().max(0.001)
            );
        }
    }
    flush(&mut batch, &mut have)?;
    for h in handles {
        let _ = h.join();
    }
    if progress {
        eprintln!(
            "embeddings: {} done, {skipped} unreadable, {:.1} s",
            done - skipped,
            t0.elapsed().as_secs_f64()
        );
    }
    Ok(have)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args
        .iter()
        .any(|a| matches!(a.as_str(), "--version" | "-V"))
    {
        memetag::version("memetag-infer");
        return;
    }
    if args.as_slice() != ["--request"] {
        eprintln!("Use memetag embeddings [--limit N]; memetag-infer accepts --version and internal --request");
        std::process::exit(1);
    }
    let result = (|| -> Result<(), String> {
        let request: memetag::propose::InferenceRequest =
            serde_json::from_reader(std::io::stdin()).map_err(|e| e.to_string())?;
        let mut c = memetag::cfg();
        c.root = request.root;
        c.sources = request.sources.map(Ok);
        c.active_source = None;
        c.db = request.db;
        c.thumbs = request.thumbs;
        c.embed_model = request.embed_model;
        c.index_threads = request.index_threads;
        let db = Db::open_cfg(&c)?;
        ensure(&c, &db, &request.rows, request.limit, request.progress)?;
        Ok(())
    })();
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
