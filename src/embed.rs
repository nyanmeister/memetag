//! Copy saved OCR text into files, without inference or writes to the source index.
use crate::{containers, tagger, xmp, Cfg};
use rusqlite::{Connection, OpenFlags};
use std::path::{Component, Path};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let mut c = c.clone();
    let (mut limit, mut dry_run) = (usize::MAX, false);
    let mut override_root = false;
    let mut selected_source = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => c.db = args.next().ok_or("--db needs a path")?.into(),
            "--root" => {
                c.root = args.next().ok_or("--root needs a path")?.into();
                override_root = true;
            }
            "--source" => {
                selected_source = Some(args.next().ok_or("--source needs an ID")?.clone())
            }
            "--limit" => {
                limit = args
                    .next()
                    .ok_or("--limit needs a number")?
                    .parse()
                    .map_err(|_| "invalid limit")?
            }
            "--dry-run" => dry_run = true,
            _ => return Err(format!("unknown embed-text option {arg}")),
        }
    }
    let db = Connection::open_with_flags(&c.db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    // Materialize a short read snapshot, then release it before touching files.
    let rows = saved_text(&db)?;
    let schema: i64 = db
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if !matches!(schema, 0 | 2 | 3) {
        return Err(format!("Unsupported snapshot schema {schema}"));
    }
    drop(db);
    let stop = Arc::new(AtomicBool::new(false));
    let signal = stop.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::SeqCst);
    })
    .map_err(|e| e.to_string())?;
    if schema != 3 || override_root {
        c.sources = None;
        c.active_source = None;
    }
    let root = if schema != 3 || override_root {
        Some(c.root.canonicalize().map_err(|e| e.to_string())?)
    } else {
        None
    };
    let (mut written, mut skipped, mut errors, mut attempted) = (0, 0, 0, 0);
    for (rel, body) in rows {
        let (source_id, source_rel) = if schema == 3 {
            rel.split_once('/')
                .ok_or("Invalid source key in snapshot")?
        } else {
            ("main", rel.as_str())
        };
        if selected_source.as_deref().is_some_and(|id| id != source_id)
            || (schema == 3 && override_root && selected_source.is_none() && source_id != "main")
        {
            continue;
        }
        if stop.load(Ordering::SeqCst) || attempted >= limit {
            break;
        }
        let result: Result<bool, String> = (|| {
            let path = if let Some(root) = &root {
                safe_path(root, source_rel)?
            } else {
                let library = c
                    .library()?
                    .ok_or("Schema 3 needs its source configuration or --root --source")?;
                let source = library.source(source_id)?;
                if !source.enabled || !source.scope.contains(Path::new(source_rel)) {
                    return Ok(false);
                }
                c.for_file(&source.path.join(source_rel))?;
                safe_path(
                    &source.path.canonicalize().map_err(|e| e.to_string())?,
                    source_rel,
                )?
            };
            if already_embedded(&path, &body)? {
                return Ok(false);
            }
            attempted += 1;
            if dry_run {
                println!("would embed: {rel}");
            } else {
                tagger::set_cached_text(&c, &path, &body)?;
            }
            Ok(true)
        })();
        match result {
            Ok(true) => written += 1,
            Ok(false) => skipped += 1,
            Err(e) => {
                errors += 1;
                eprintln!("embed failed {rel}: {e}");
            }
        }
    }
    println!(
        "embed-text: {written} {}, {skipped} already match, {errors} errors{}",
        if dry_run { "would embed" } else { "embedded" },
        if stop.load(Ordering::SeqCst) {
            " — stopped; rerun to continue"
        } else {
            ""
        }
    );
    if errors > 0 {
        Err("some files failed; rerun to retry (matching files are skipped)".into())
    } else {
        Ok(())
    }
}

fn saved_text(db: &Connection) -> Result<Vec<(String, String)>, String> {
    let mut st = db.prepare("SELECT t.path, t.body FROM text t JOIN files f ON f.path=t.path WHERE f.kind='image' AND length(t.body)>0 ORDER BY t.path").map_err(|e| e.to_string())?;
    let rows = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn safe_path(root: &Path, rel: &str) -> Result<std::path::PathBuf, String> {
    if !Path::new(rel)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return Err("index path must be relative without parent components".into());
    }
    let path = root.join(rel).canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(root) {
        return Err("index path resolves outside the collection".into());
    }
    Ok(path)
}

fn already_embedded(path: &Path, body: &str) -> Result<bool, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let parsed = containers::get_xmp(&bytes)?
        .map(|p| xmp::read(&p))
        .transpose()?;
    // Never replace a human correction with an older machine-text snapshot.
    Ok(parsed
        .as_ref()
        .map(|p| p.fields.get("textSource").map(String::as_str) == Some("manual"))
        .unwrap_or(false)
        || parsed
            .and_then(|p| p.fields.get("text").cloned())
            .as_deref()
            == Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, FileTimes};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn cached_text_roundtrip_preserves_file_and_retries() {
        let root = std::env::temp_dir().join(format!("memetag-embed-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let c = Cfg {
            db: root.join("source.sqlite"),
            vocab: crate::vocab::Vocab::load(&root.join("no-vocab")),
            ..Cfg::for_tests(&root)
        };
        let db = crate::index::Db::open_cfg(&c).unwrap();
        for ext in ["png", "jpg", "gif", "webp"] {
            let path = root.join(format!("test.{ext}"));
            image::RgbImage::from_pixel(8, 8, image::Rgb([90, 40, 10]))
                .save(&path)
                .unwrap();
            let time = UNIX_EPOCH + Duration::new(1_500_000_000, 123_456_789);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
            db.upsert_file(&root, &path, &c.vocab).unwrap();
            let rel = format!("test.{ext}");
            db.conn
                .execute(
                    "INSERT INTO text VALUES(?1, ?2)",
                    (&rel, "Saved café caption AND more"),
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO text_meta VALUES(?1, 'ollama:already-done', 1, 1)",
                    [&rel],
                )
                .unwrap();
            let old = fs::metadata(&path).unwrap();
            let pixels = crate::writer::pixel_hash(&fs::read(&path).unwrap());
            let body = "Saved café caption AND more";
            assert!(!already_embedded(&path, body).unwrap());
            tagger::set_cached_text(&c, &path, body).unwrap();
            let new = fs::metadata(&path).unwrap();
            assert_eq!(new.modified().unwrap(), time);
            assert_eq!(old.ino(), new.ino());
            assert_eq!(old.created().ok(), new.created().ok());
            assert_eq!(pixels, crate::writer::pixel_hash(&fs::read(&path).unwrap()));
            assert!(already_embedded(&path, body).unwrap());
            let bytes = fs::read(&path).unwrap();
            if !already_embedded(&path, body).unwrap() {
                tagger::set_cached_text(&c, &path, body).unwrap();
            }
            assert_eq!(bytes, fs::read(&path).unwrap());
            assert_eq!(new.ctime(), fs::metadata(&path).unwrap().ctime());
            assert!(tagger::set_cached_text(&c, &root.join("missing.png"), body).is_err());
            // A newer result can be embedded on a later run, still preserving mtime.
            tagger::set_cached_text(&c, &path, "A corrected caption").unwrap();
            assert!(already_embedded(&path, "A corrected caption").unwrap());
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        }
        assert_eq!(saved_text(&db.conn).unwrap().len(), 4);
        assert!(saved_text(&db.conn)
            .unwrap()
            .iter()
            .all(|(_, text)| text == "Saved café caption AND more"));
        assert!(safe_path(&root, "../outside").is_err());
        assert!(safe_path(&root, "/etc/passwd").is_err());
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }
}
