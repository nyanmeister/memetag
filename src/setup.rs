use crate::Cfg;
use std::{fs, io::Write, path::Path};

pub fn init(root: &str) -> Result<(), String> {
    let root = Path::new(root)
        .canonicalize()
        .map_err(|e| format!("collection root: {e}"))?;
    if !root.is_dir() {
        return Err("collection root must be a directory".into());
    }
    let dir = crate::paths::config_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("config.toml");
    let mut table = toml::Table::new();
    table.insert("root".into(), root.to_string_lossy().into_owned().into());
    let text = toml::to_string_pretty(&table).map_err(|e| e.to_string())?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}; existing configuration is kept", path.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|e| e.to_string())?;
    println!("Created {}\nChoose folders with `memetag folders menu`, then run `memetag reindex` and `memetag thumbs`.", path.display());
    Ok(())
}
pub fn doctor(c: &Cfg) -> Result<(), String> {
    crate::version("memetag");
    println!(
        "config: {}\nroot: {}\nindex: {}\nthumbnails: {}",
        crate::paths::config_dir().display(),
        c.root.display(),
        c.db.display(),
        c.thumbs.display()
    );
    println!("xmp namespace: {}", crate::xmp::meme_ns());
    if let Some(l) = crate::xmp::legacy_ns().filter(|l| !l.is_empty()) {
        println!("legacy xmp namespaces: {}", l.join(", "));
    }
    for (table, key) in misplaced_namespace_keys(&crate::paths::config_table().unwrap_or_default())
    {
        println!("WARNING: {key} sits inside [{table}] in config.toml and has no effect there; move it above the first [table] header");
    }
    for (name, purpose) in [
        ("memetag-gui", "browser and folder menu"),
        ("memetag-infer", "new image embeddings"),
        ("ffmpeg", "video previews, AVIF and speech extraction"),
        ("ffprobe", "video information"),
        ("feh", "still-image viewer"),
        ("mpv", "animated/video viewer"),
        ("tesseract", "local OCR"),
        ("fc-match", "CJK font discovery"),
        ("systemctl", "optional background services"),
    ] {
        println!(
            "{name}: {} ({purpose})",
            if crate::companion(name).is_ok() {
                "found"
            } else {
                "missing"
            }
        );
    }
    crate::sources::run(c, &[])?;
    println!(
        "clipboard: X11/XWayland (DISPLAY={})",
        std::env::var("DISPLAY").unwrap_or_default()
    );
    Ok(())
}

/// Namespace keys that landed under a `[table]` header, where TOML files them silently: a desktop config carried
/// `xmp_namespace` below `[pull_remote]` from 2026-10-05 to 2026-10-24 and wrote the public default all that time.
/// Pairs of (table, key), in file order.
pub fn misplaced_namespace_keys(t: &toml::Table) -> Vec<(String, String)> {
    let mut out = vec![];
    for (name, v) in t {
        if let Some(sub) = v.as_table() {
            for key in ["xmp_namespace", "legacy_xmp_namespaces"] {
                if sub.contains_key(key) {
                    out.push((name.clone(), key.to_string()));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn namespace_keys_under_a_table_are_reported_and_top_level_ones_are_not() {
        let bad: toml::Table = "root = \"/x\"\n[pull_remote]\nhost = \"h\"\nxmp_namespace = \"urn:a\"\nlegacy_xmp_namespaces = [\"urn:b\"]\n"
            .parse()
            .unwrap();
        assert_eq!(
            misplaced_namespace_keys(&bad),
            vec![
                ("pull_remote".to_string(), "xmp_namespace".to_string()),
                (
                    "pull_remote".to_string(),
                    "legacy_xmp_namespaces".to_string()
                )
            ]
        );
        let good: toml::Table = "xmp_namespace = \"urn:a\"\n[pull_remote]\nhost = \"h\"\n"
            .parse()
            .unwrap();
        assert!(misplaced_namespace_keys(&good).is_empty());
    }
}
