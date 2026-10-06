//! The one place that writes into a file: merge our fields, write in place, verify. Used by `tag` and `ocr --embed`.
use crate::{containers, writer, xmp, Cfg};
use std::path::Path;

/// `bytes` are the verified bytes now on disk, so callers never re-read what they just wrote.
pub struct Outcome {
    pub report: writer::Report,
    pub bytes: Vec<u8>,
}

/// `ops`: "+tag" add, "-tag" remove, "tag" add. `extra_fields`: meme:* fields to set (e.g. text).
pub fn apply(
    c: &Cfg,
    path: &Path,
    ops: &[String],
    extra_fields: &[(&str, String)],
) -> Result<Outcome, String> {
    let before = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    apply_bytes(c, path, before, ops, extra_fields, true)
}

#[cfg(test)]
pub fn edit_file(
    c: &Cfg,
    path: &Path,
    ops: &[String],
    fields: &[(&str, String)],
) -> Result<(), String> {
    let before = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    edit_bytes(c, path, before, ops, fields).map(|_| ())
}

/// Edit bytes the caller already holds; returns the verified bytes now on disk.
/// One read before (the caller's) and one after (the writer's verification): nothing else touches the mount.
pub fn edit_bytes(
    c: &Cfg,
    path: &Path,
    before: Vec<u8>,
    ops: &[String],
    fields: &[(&str, String)],
) -> Result<Vec<u8>, String> {
    let result = apply_bytes(c, path, before, ops, fields, false)?;
    if !result.report.ok() {
        return Err("Verification failed: pixels, timestamps or inode".into());
    }
    let packet = containers::get_xmp(&result.bytes)?.ok_or("Missing saved metadata")?;
    let parsed = xmp::read(&packet)?;
    for (key, value) in fields {
        if parsed.fields.get(*key) != Some(value) {
            return Err(format!("Saved {key} did not round-trip"));
        }
    }
    Ok(result.bytes)
}

pub fn apply_bytes(
    c: &Cfg,
    path: &Path,
    before: Vec<u8>,
    ops: &[String],
    extra_fields: &[(&str, String)],
    refresh_index: bool,
) -> Result<Outcome, String> {
    apply_bytes_impl(c, path, before, ops, extra_fields, refresh_index, true)
}

/// The server helper already owns the collection path; never route its write back through SSH.
pub(crate) fn apply_bytes_local(
    c: &Cfg,
    path: &Path,
    before: Vec<u8>,
    ops: &[String],
    extra_fields: &[(&str, String)],
) -> Result<Outcome, String> {
    apply_bytes_impl(c, path, before, ops, extra_fields, false, false)
}

fn apply_bytes_impl(
    c: &Cfg,
    path: &Path,
    before: Vec<u8>,
    ops: &[String],
    extra_fields: &[(&str, String)],
    refresh_index: bool,
    route_remote: bool,
) -> Result<Outcome, String> {
    let existing = containers::get_xmp(&before)?;
    let parsed = existing
        .as_deref()
        .map(xmp::read)
        .transpose()?
        .unwrap_or_default();
    if parsed.fields.get("textSource").map(String::as_str) == Some("manual")
        && extra_fields.iter().any(|(k, _)| *k == "text")
        && !extra_fields.iter().any(|(k, _)| *k == "textSource")
    {
        return Err("Human-reviewed OCR text is protected".into());
    }
    let mut tags: std::collections::BTreeSet<String> =
        parsed.tags.iter().map(|t| c.vocab.canon(t)).collect();
    for op in ops {
        match op.strip_prefix('+') {
            Some(t) => {
                tags.insert(c.vocab.canon(t));
            }
            None => match op.strip_prefix('-') {
                Some(t) => {
                    tags.remove(&c.vocab.canon(t));
                }
                None => {
                    tags.insert(c.vocab.canon(op));
                }
            },
        }
    }
    // a blank tag (`memetag tag x "+  "`) is nothing to write: the reader drops an empty entry, so the round-trip
    // check would fail the whole write (found by the exiftool differential run, 2026-09-22)
    tags.remove("");
    let md = std::fs::metadata(path).map_err(|e| e.to_string())?;
    let mut fields = parsed.fields.clone();
    // the id is the pixel hash where pixels exist: recompute on every tag so an XMP packet copied by another tool (ImageMagick does this) cannot carry a stale id
    match writer::pixel_hash(&before) {
        Some(h) => {
            fields.insert("id".into(), h);
        }
        None => {
            fields
                .entry("id".into())
                .or_insert_with(|| writer::content_id(&before));
        }
    }
    fields
        .entry("origMtime".into())
        .or_insert_with(|| md.modified().map(writer::ts).unwrap_or_default());
    fields
        .entry("origBtime".into())
        .or_insert_with(|| md.created().map(writer::ts).unwrap_or_default());
    fields.insert("taggedAt".into(), writer::now_s());
    for (k, v) in extra_fields {
        fields.insert((*k).to_string(), v.clone());
    }
    let tag_vec: Vec<String> = tags.iter().cloned().collect();
    let packet = xmp::merge(existing.as_deref(), &tag_vec, &fields)?;
    let after = containers::set_xmp(&before, &packet)?;
    let remote = if route_remote {
        crate::batch::write_remote(c, path, &before, &after, &packet)?
    } else {
        None
    };
    let writer::Written { report, bytes: re } = match remote {
        Some(written) => written,
        None => writer::write_in_place(path, &before, &after)?,
    };
    let got = containers::get_xmp(&re)?
        .map(|p| xmp::read(&p))
        .transpose()?
        .map(|r| r.tags)
        .unwrap_or_default();
    if got != tag_vec {
        return Err(format!("tags did not round-trip in {}", path.display()));
    }
    // keep the index warm if the file lives under the root (from the bytes just verified: no extra read); a write
    // that set no text leaves the index's machine text alone, as the mass edit and the editor do
    if refresh_index {
        if let Ok(db) = crate::index::Db::open_cfg(&c) {
            if c.file_key(path).is_ok() {
                let _ = if extra_fields.iter().any(|(k, _)| *k == "text") {
                    db.upsert_cfg(c, path, &re).map(|_| ())
                } else {
                    std::fs::metadata(path)
                        .map_err(|e| e.to_string())
                        .and_then(|md| db.import_tag_scan(c.scan_bytes(path, &re, &md)?))
                };
            }
        }
    }
    Ok(Outcome { report, bytes: re })
}

/// Embed a cached result without modifying its source database (which may be a
/// snapshot, or may have a newer OCR result arriving concurrently).
pub fn set_cached_text(c: &Cfg, path: &Path, text: &str) -> Result<(), String> {
    let before = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let o = apply_bytes(c, path, before, &[], &[("text", text.to_string())], false)?;
    if !o.report.ok() {
        return Err("verification failed (pixels, timestamps or inode)".into());
    }
    let packet = containers::get_xmp(&o.bytes)?.ok_or("missing XMP after embedding")?;
    if xmp::read(&packet)?.fields.get("text").map(String::as_str) != Some(text) {
        return Err("OCR text did not round-trip".into());
    }
    Ok(())
}

pub fn set_field(c: &Cfg, path: &Path, key: &str, value: &str) -> Result<(), String> {
    let o = apply(c, path, &[], &[(key, value.to_string())])?;
    if o.report.ok() {
        Ok(())
    } else {
        Err("verification failed".into())
    }
}
