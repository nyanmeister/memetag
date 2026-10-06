//! `memetag dupes [--distance N]` — near-duplicate groups by perceptual hash (see `similar` for the hash and its cache).
//! `memetag export` — one JSON object per line for every indexed file (for import into other tools).
use crate::index::Db;
use crate::Cfg;

pub fn dupes(c: &Cfg, max_distance: u32) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let before = crate::similar::cached(&db)?.len();
    let mut hashes: Vec<(String, Vec<u8>)> = crate::similar::ensure(c, &db, &rows, true)?
        .into_iter()
        .collect();
    // by path, so equal-distance ties break the same way on every run (a HashMap's order does not)
    hashes.sort_by(|a, b| a.0.cmp(&b.0));
    let new = hashes.len().saturating_sub(before);
    let groups = crate::similar::groups(
        &hashes.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
        max_distance,
    );
    let by_path: std::collections::HashMap<&str, &crate::index::FileRow> =
        rows.iter().map(|r| (r.path.as_str(), r)).collect();
    for (k, members) in groups.iter().enumerate() {
        println!("group {}:", k + 1);
        for &i in members {
            if let Some(r) = by_path.get(hashes[i].0.as_str()) {
                println!(
                    "  {}  {}x{}  {} tags  {}",
                    r.path, r.width, r.height, r.xmp_tag_count, r.format
                );
            }
        }
    }
    let grouped: usize = groups.iter().map(Vec::len).sum();
    println!("dupes: {} images hashed ({new} new), {} groups holding {grouped} files (every member within {max_distance} bits of every other)", hashes.len(), groups.len());
    Ok(())
}

pub fn export(c: &Cfg) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    let mut sources: std::collections::HashMap<String, Vec<(String, String)>> = Default::default();
    {
        let mut st = db
            .conn
            .prepare("SELECT path, tag, source FROM tags")
            .map_err(|e| e.to_string())?;
        for r in st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
        {
            sources.entry(r.0).or_default().push((r.1, r.2));
        }
    }
    let out = std::io::stdout();
    let mut w = std::io::BufWriter::new(out.lock());
    use std::io::Write;
    for r in &rows {
        let ts = sources.get(&r.path).cloned().unwrap_or_default();
        let list = |src: &str| -> Vec<&str> {
            ts.iter()
                .filter(|(_, s)| s == src)
                .map(|(t, _)| t.as_str())
                .collect()
        };
        // serde does the escaping: a tab, a carriage return or a control character in reviewed text or a file
        // name made a hand-built line invalid JSON (review, 2026-09-22); export must remain valid for downstream importers
        let line = serde_json::json!({
            "id": r.id, "path": r.path, "format": r.format, "kind": r.kind,
            "width": r.width, "height": r.height, "size": r.size,
            "created_at": r.created_at, "tagged_at": r.tagged_at,
            "tags": list("xmp"), "folder_tags": list("folder"), "implied_tags": list("implied"),
            "text": r.text,
        });
        serde_json::to_writer(&mut w, &line).map_err(|e| e.to_string())?;
        writeln!(w).map_err(|e| e.to_string())?;
    }
    Ok(())
}
