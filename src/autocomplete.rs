//! Supplemental vocabulary credit: Derpibooru public tag API, backed by
//! Philomena (https://github.com/philomena-dev/philomena/blob/master/openapi.yaml).
//! Names only, suggestions only: no external implications, aliases or tags are
//! applied automatically. No network requests are made while typing.
use crate::{
    index::{Db, FileRow},
    Cfg,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read;

#[derive(Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub name: String,
    #[serde(default)]
    pub images: u64,
}

pub fn cache_path(c: &Cfg) -> std::path::PathBuf {
    c.db.with_file_name("supplemental-tags.json")
}

pub fn load(c: &Cfg) -> Vec<(String, u64, bool)> {
    let mut values: HashMap<String, (u64, bool)> = HashMap::new();
    if let Ok(db) = Db::open_cfg(&c) {
        for (tag, n) in db.tag_counts().unwrap_or_default() {
            if !tag.starts_with("folder:") {
                values.insert(tag, (n as u64, true));
            }
        }
        if let Ok(mut st) = db.conn.prepare("SELECT tag,uses FROM tag_history") {
            if let Ok(rows) =
                st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            {
                for (tag, n) in rows.flatten() {
                    let item = values.entry(tag).or_insert((0, true));
                    item.0 += n.max(0) as u64;
                }
            }
        }
    }
    for tag in c.vocab.aliases.keys().chain(c.vocab.aliases.values()) {
        values.entry(tag.clone()).or_insert((0, true));
    }
    // a rule's tag and what it implies are vocabulary too, before any file carries them
    for tag in c
        .vocab
        .implications
        .keys()
        .chain(c.vocab.implications.values().flatten())
    {
        values.entry(tag.clone()).or_insert((0, true));
    }
    // action tags are consumed by their pass, so no count keeps them here; they are offered regardless
    for a in crate::meta::ACTIONS {
        values.entry(a.request.to_string()).or_insert((0, true));
    }
    if let Ok(bytes) = std::fs::read(cache_path(c)) {
        if let Ok(tags) = serde_json::from_slice::<Vec<Suggestion>>(&bytes) {
            for tag in tags {
                values.entry(tag.name).or_insert((tag.images, false));
            }
        }
    }
    values
        .into_iter()
        .map(|(tag, (count, own))| (tag, count, own))
        .collect()
}

/// Live row counts merged with remembered local vocabulary for both search bars.
/// History can keep a tag visible after its last file disappears; it is a floor
/// for suggestion ranking, not an additional count of files.
pub(crate) fn local_counts(
    rows: &[FileRow],
    remembered: &[(String, u64, bool)],
) -> Vec<(String, u64, bool)> {
    let mut counts = HashMap::<String, u64>::new();
    for row in rows {
        for tag in &row.tags {
            *counts.entry(tag.clone()).or_default() += 1;
        }
    }
    for (tag, count, _) in remembered {
        let value = counts.entry(tag.clone()).or_default();
        *value = (*value).max(*count);
    }
    counts
        .into_iter()
        .map(|(tag, count)| (tag, count, true))
        .collect()
}

pub fn matches(values: &[(String, u64, bool)], query: &str) -> Vec<String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return vec![];
    }
    let mut hits: Vec<_> = values
        .iter()
        .filter(|(tag, _, _)| tag.to_lowercase().contains(&q))
        .collect();
    hits.sort_by_key(|(tag, n, own)| {
        (
            !*own,
            !tag.to_lowercase().starts_with(&q),
            std::cmp::Reverse(*n),
            tag,
        )
    });
    hits.into_iter()
        .take(8)
        .map(|(name, _, _)| name.clone())
        .collect()
}

pub fn refresh(c: &Cfg) -> Result<(), String> {
    // Bounded popular seed vocabulary, not a crawl of every tag on the site.
    let mut tags: HashMap<String, Suggestion> = HashMap::new();
    for page in 1..=20 {
        let mut attempts = 0;
        let response = loop {
            let result = ureq::get("https://derpibooru.org/api/v1/json/search/tags")
                .query("q", "*")
                .query("per_page", "50")
                .query("page", &page.to_string())
                .set("User-Agent", "memetag/0.1 vocabulary cache")
                .timeout(std::time::Duration::from_secs(20))
                .call();
            if matches!(&result, Err(ureq::Error::Status(429, _))) && attempts < 3 {
                attempts += 1;
                eprintln!("Vocabulary: server requested a pause; retrying shortly");
                std::thread::sleep(std::time::Duration::from_secs(10 * attempts));
                continue;
            }
            break result.map_err(|e| match e {
                ureq::Error::Status(code, _) => format!("Vocabulary download failed (HTTP {code})"),
                ureq::Error::Transport(_) => {
                    "Vocabulary download failed; check your connection and retry".into()
                }
            })?;
        };
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(16 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        #[derive(Deserialize)]
        struct Response {
            tags: Vec<Suggestion>,
        }
        let result: Response =
            serde_json::from_slice(&bytes).map_err(|e| format!("Vocabulary response: {e}"))?;
        if result.tags.is_empty() {
            break;
        }
        for tag in result.tags {
            if !tag.name.is_empty() && tag.name.len() <= 200 {
                tags.insert(tag.name.clone(), tag);
            }
        }
        eprintln!("Vocabulary: {} suggestions", tags.len());
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let mut tags: Vec<_> = tags.into_values().collect();
    tags.sort_by(|a, b| a.name.cmp(&b.name));
    let path = cache_path(c);
    std::fs::create_dir_all(path.parent().ok_or("Missing cache directory")?)
        .map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&tags).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    println!("Cached {} supplemental tag suggestions", tags.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn previous_entries_rank_before_supplemental_and_match_case() {
        let values = vec![
            ("cat".into(), 999_999, false),
            ("my cat tag".into(), 1, true),
            ("category".into(), 2, true),
        ];
        assert_eq!(
            matches(&values, "CAT"),
            vec!["category", "my cat tag", "cat"]
        );
        assert!(matches(&values, "").is_empty());
    }
    #[test]
    fn rules_and_their_targets_are_offered_before_any_file_has_them() {
        let dir = std::env::temp_dir().join(format!("memetag-ac-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut vocab = crate::vocab::Vocab::load(&dir.join("no-vocab"));
        vocab.implications.insert(
            "stonetoss comic".into(),
            vec!["comic".into(), "artist:stonetoss".into()],
        );
        let c = Cfg {
            vocab,
            ..Cfg::for_tests(&dir)
        };
        let values = load(&c);
        for want in ["stonetoss comic", "comic", "artist:stonetoss"] {
            let hit = values
                .iter()
                .find(|(t, _, _)| t == want)
                .unwrap_or_else(|| panic!("{want} missing"));
            assert!(hit.2, "{want} counts as the collection's own vocabulary");
        }
        assert_eq!(
            matches(&values, "stonet"),
            ["stonetoss comic", "artist:stonetoss"],
            "a prefix match ranks first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
