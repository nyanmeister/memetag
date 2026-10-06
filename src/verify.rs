//! `memetag verify [--deep]` — audit every indexed file: readable, XMP parses, tags/ids consistent with the index,
//! mtime unchanged since tagging (origMtime), and (--deep) decoded pixels still hash to the stored id.
use crate::index::Db;
use crate::{containers, writer, xmp, Cfg};

pub fn run(c: &Cfg, deep: bool) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let (
        mut n,
        mut missing,
        mut unreadable,
        mut bad_xmp,
        mut id_mismatch,
        mut mtime_moved,
        mut tag_drift,
        mut pixel_mismatch,
        mut undecodable,
    ) = (0, 0, 0, 0, 0, 0, 0, 0, 0);
    {
        let mut by_id: std::collections::HashMap<&str, Vec<&str>> = Default::default();
        for r in &rows {
            by_id
                .entry(r.id.as_str())
                .or_default()
                .push(r.path.as_str());
        }
        for (id, ps) in by_id.iter().filter(|(_, ps)| ps.len() > 1) {
            println!("SHARED-ID {id}: {}  (identical content, or XMP copied across a re-encode — `tag` refreshes the id)", ps.join("  ·  "));
        }
    }
    for r in &rows {
        n += 1;
        let path = c.file_path(&r.path)?;
        let Ok(bytes) = std::fs::read(&path) else {
            if !path.exists() {
                missing += 1;
                println!("MISSING   {}", r.path);
            } else {
                unreadable += 1;
                println!("UNREADABLE {}", r.path);
            }
            continue;
        };
        let packet = match containers::get_xmp(&bytes) {
            Ok(p) => p,
            Err(e) => {
                bad_xmp += 1;
                println!("BAD-CONTAINER {} ({e})", r.path);
                continue;
            }
        };
        let parsed = match packet.as_deref().map(xmp::read).transpose() {
            Ok(p) => p.unwrap_or_default(),
            Err(e) => {
                bad_xmp += 1;
                println!("BAD-XMP   {} ({e})", r.path);
                continue;
            }
        };
        if let Some(id) = parsed.fields.get("id") {
            if id != &r.id {
                id_mismatch += 1;
                println!("ID-DRIFT  {} file={id} index={}", r.path, r.id);
            }
        }
        if let Some(om) = parsed
            .fields
            .get("origMtime")
            .and_then(|s| s.parse::<f64>().ok())
        {
            if (om - r.mtime).abs() > 0.5 {
                mtime_moved += 1;
                println!("MTIME-MOVED {} orig={om:.0} now={:.0}", r.path, r.mtime);
            }
        }
        let file_tags: std::collections::BTreeSet<String> =
            parsed.tags.iter().map(|t| c.vocab.canon(t)).collect();
        let index_xmp: std::collections::BTreeSet<String> = {
            let mut st = db
                .conn
                .prepare("SELECT tag FROM tags WHERE path=?1 AND source='xmp'")
                .map_err(|e| e.to_string())?;
            let v: std::collections::BTreeSet<String> = st
                .query_map([&r.path], |x| x.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .filter_map(Result::ok)
                .collect();
            v
        };
        if file_tags != index_xmp {
            tag_drift += 1;
            println!(
                "TAG-DRIFT {} (file {} vs index {}) — run reindex",
                r.path,
                file_tags.len(),
                index_xmp.len()
            );
        }
        if deep && r.kind == "image" {
            match writer::pixel_hash(&bytes) {
                Some(h) => {
                    if let Some(id) = parsed.fields.get("id") {
                        if id != &h {
                            pixel_mismatch += 1;
                            println!("PIXELS-CHANGED {} id={id} now={h}", r.path);
                        }
                    }
                }
                None => {
                    undecodable += 1;
                    println!("UNDECODABLE {}", r.path);
                }
            }
        }
    }
    println!("verify: {n} files · missing {missing} · unreadable {unreadable} · bad xmp/container {bad_xmp} · id drift {id_mismatch} · mtime moved {mtime_moved} · tag drift {tag_drift}{}",
        if deep { format!(" · pixels changed {pixel_mismatch} · undecodable {undecodable}") } else { String::new() });
    if missing + unreadable + bad_xmp + id_mismatch + mtime_moved + tag_drift + pixel_mismatch > 0 {
        Err("verify found problems".into())
    } else {
        Ok(())
    }
}
