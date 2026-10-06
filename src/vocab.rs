//! Aliases and implications, derpibooru style, from ~/.config/memetag/vocab.toml:
//!   [aliases]       "thisisfine" = "this is fine"          (query and file tags are canonicalised)
//!   [implications]  "character:reimu" = ["series:touhou"]  (added as derived tags at index time)
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Deserialize, serde::Serialize, Default, Debug, Clone)]
pub struct Vocab {
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub implications: HashMap<String, Vec<String>>,
    /// set when the file existed but did not parse: `save` refuses, so a typo never costs the whole vocabulary
    #[serde(skip)]
    pub broken: Option<String>,
    /// where `load` read this from, so `save` can put it back; None for the built-in empty vocabulary
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// Exact snapshot read from disk, for optimistic saves under the shared local lock.
    #[serde(skip)]
    pub original: Option<Vec<u8>>,
}

/// What `rename` did: whether anything changed, and rules of the old tag that were not carried onto the new one.
#[derive(Default, Debug)]
pub struct Renamed {
    pub changed: bool,
    pub not_carried: Vec<String>,
}

impl Vocab {
    pub fn load(path: &Path) -> Vocab {
        let mut v = Vocab::default();
        match Self::read_bytes(path) {
            Ok(bytes) => {
                if let Some(b) = &bytes {
                    match std::str::from_utf8(b)
                        .map_err(|e| e.to_string())
                        .and_then(|s| toml::from_str::<Vocab>(s).map_err(|e| e.to_string()))
                    {
                        Ok(parsed) => v = parsed,
                        Err(e) => {
                            eprintln!("vocab: {e}");
                            v.broken = Some(e.to_string());
                        }
                    }
                }
                v.original = bytes;
            }
            Err(e) => v.broken = Some(e),
        }
        v.path = Some(path.to_path_buf());
        v
    }
    /// Write the vocabulary back where it came from. The comment block above the first table survives; the tables are
    /// rewritten sorted, so any comment inside them is lost (none there today).
    pub fn save(&mut self) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .ok_or("this vocabulary was not loaded from a file")?;
        if let Some(e) = &self.broken {
            return Err(format!(
                "{} did not parse ({e}); fix it by hand before saving, or every rule and alias in it would be lost",
                path.display()
            ));
        }
        let _lock = Self::lock(path)?;
        let current = Self::read_bytes(path)?;
        if current != self.original {
            return Err("Rules changed since editing began. Close and reopen the Implications card before saving.".into());
        }
        let old = String::from_utf8(current.unwrap_or_default()).map_err(|e| e.to_string())?;
        let header: String = old
            .lines()
            .take_while(|l| !l.trim_start().starts_with('['))
            .map(|l| format!("{l}\n"))
            .collect();
        let body = self.normalized_body();
        let tmp = path.with_extension("toml.tmp");
        let bytes = format!("{header}{body}").into_bytes();
        std::fs::write(&tmp, &bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))?;
        self.original = Some(bytes);
        Ok(())
    }
    pub fn lock(path: &Path) -> Result<crate::locking::Lock, String> {
        crate::locking::sidecar(&path.with_extension("toml.lock"))
    }
    pub fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("read {}: {e}", path.display())),
        }
    }
    /// The tables as `save` writes them — aliases then implications, both sorted, empty rules dropped, each rule's
    /// targets sorted and deduplicated — without the header comment. The sync fingerprints a hash of exactly this,
    /// so formatting, comments and key order never make two identical rule sets look different (only the rules do).
    /// Infallible: `toml::to_string` of a map with string keys and string/list values cannot fail.
    pub fn normalized_body(&self) -> String {
        #[derive(serde::Serialize)]
        struct Sorted<'a> {
            aliases: BTreeMap<&'a str, &'a str>,
            implications: BTreeMap<&'a str, Vec<&'a str>>,
        }
        let sorted = Sorted {
            aliases: self
                .aliases
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            implications: self
                .implications
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| {
                    let mut v: Vec<&str> = v.iter().map(String::as_str).collect();
                    v.sort_unstable();
                    v.dedup();
                    (k.as_str(), v)
                })
                .collect(),
        };
        toml::to_string(&sorted).unwrap_or_default()
    }
    /// Trimmed, lowercased in every alphabet (as the text search folds case), then through the aliases; the alias
    /// target is normalised the same way, so a hand-written `"pg" = "Peter Griffin"` cannot put two spellings of one
    /// tag into a file (review, 2026-09-22). No hand-written tag in the collection had a non-ASCII letter that day.
    pub fn canon(&self, tag: &str) -> String {
        let t = tag.trim().to_lowercase();
        match self.aliases.get(&t) {
            Some(target) => target.trim().to_lowercase(),
            None => t,
        }
    }
    /// canonical tags plus everything they imply, transitively
    pub fn expand(&self, tags: impl IntoIterator<Item = String>) -> BTreeSet<String> {
        let mut out: BTreeSet<String> = tags.into_iter().map(|t| self.canon(&t)).collect();
        let mut queue: Vec<String> = out.iter().cloned().collect();
        while let Some(t) = queue.pop() {
            if let Some(im) = self.implications.get(&t) {
                for x in im {
                    let x = self.canon(x);
                    if out.insert(x.clone()) {
                        queue.push(x);
                    }
                }
            }
        }
        out
    }
    /// Does `from` already lead to `to`, directly or through other rules? Both canonical.
    pub fn implies(&self, from: &str, to: &str) -> bool {
        from != to && self.expand([from.to_string()]).contains(to)
    }
    /// Every rule as (specific, general) pairs, sorted, for listing.
    pub fn rules(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .implications
            .iter()
            .flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone())))
            .collect();
        v.sort();
        v.dedup();
        v
    }
    pub fn add_rule(&mut self, from: &str, to: &str) {
        let e = self.implications.entry(from.to_string()).or_default();
        if !e.iter().any(|x| x == to) {
            e.push(to.to_string());
        }
    }
    pub fn remove_rule(&mut self, from: &str, to: &str) {
        if let Some(e) = self.implications.get_mut(from) {
            e.retain(|x| x != to);
            if e.is_empty() {
                self.implications.remove(from);
            }
        }
    }
    /// A tag was renamed in the files (the Tags card): every rule keyed on the old name, every rule pointing at
    /// it and every alias resolving to it now say the new name. Renaming onto a tag that has rules of its own
    /// merges the two lists. A rule that would then say "A implies A" is dropped. Returns whether anything
    /// changed, so the caller knows whether to save and reimply. Both names canonical.
    pub fn rename(&mut self, from: &str, to: &str) -> Renamed {
        let mut r = Renamed::default();
        if from == to {
            return r;
        }
        if let Some(list) = self.implications.remove(from) {
            r.changed = true;
            // A rename carries the tag's rules to its new name. A merge onto a tag the vocabulary already knows
            // (a rule of its own, the target of one, an alias) leaves them behind: "twitter post implies twitter"
            // carried onto "twitter" would make every twitter file imply itself, and onto "social media" a cycle
            // (review, 2026-09-22). Named in the toast; the Implications card is where such rules are re-made.
            let known = self.implications.contains_key(to)
                || self.implications.values().flatten().any(|t| t == to)
                || self.aliases.contains_key(to)
                || self.aliases.values().any(|t| t == to);
            if known {
                r.not_carried = list;
            } else {
                for t in list {
                    if t == to {
                        r.not_carried.push(t);
                    } else {
                        self.implications.entry(to.to_string()).or_default().push(t);
                    }
                }
            }
        }
        for (key, list) in self.implications.iter_mut() {
            let before = list.len();
            let had_old = list.iter().any(|x| x == from);
            for t in list.iter_mut() {
                if t == from {
                    *t = to.to_string();
                }
            }
            let mut seen = BTreeSet::new();
            list.retain(|t| t != key && seen.insert(t.clone()));
            r.changed |= had_old || list.len() != before;
        }
        self.implications.retain(|_, list| !list.is_empty());
        for target in self.aliases.values_mut() {
            if target == from {
                *target = to.to_string();
                r.changed = true;
            }
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vocab(src: &str) -> Vocab {
        toml::from_str(src).unwrap()
    }
    #[test]
    fn rename_follows_keys_targets_and_aliases() {
        let mut v = vocab(
            "[aliases]\n\"pg\" = \"peter griffon\"\n[implications]\n\"peter griffon\" = [\"family guy\"]\n\"stewie\" = [\"peter griffon\", \"family guy\"]\n",
        );
        assert!(v.rename("peter griffon", "peter griffin").changed);
        assert!(v.implies("peter griffin", "family guy"));
        assert!(!v.implications.contains_key("peter griffon"));
        assert_eq!(
            v.implications["stewie"],
            vec!["peter griffin", "family guy"]
        );
        assert_eq!(v.canon("pg"), "peter griffin");
        v.aliases.insert("pg2".into(), " Peter Griffin ".into());
        assert_eq!(
            v.canon("PG2"),
            "peter griffin",
            "alias targets are normalised too"
        );
        assert_eq!(
            v.canon(&v.canon("pg2")),
            v.canon("pg2"),
            "canon is idempotent"
        );
        assert_eq!(v.canon("ÜNÏCODE Tag"), "ünïcode tag");
        assert!(!v.rename("peter griffin", "peter griffin").changed);
        assert!(!v.rename("nobody", "anybody").changed);
    }
    #[test]
    fn rename_onto_existing_tag_merges_and_drops_self_rules() {
        let mut v = vocab(
            "[implications]\n\"a\" = [\"x\", \"b\"]\n\"b\" = [\"y\"]\n\"c\" = [\"a\", \"b\"]\n",
        );
        let r = v.rename("a", "b");
        assert!(r.changed);
        assert!(!v.implications.contains_key("a"));
        assert_eq!(
            v.implications["b"],
            vec!["y"],
            "b keeps its own rules; a's are not carried onto it"
        );
        assert_eq!(r.not_carried, vec!["x", "b"]);
        assert_eq!(v.implications["c"], vec!["b"]); // deduplicated
    }
    #[test]
    fn rename_never_carries_a_rule_that_would_loop() {
        let mut v = vocab(
            "[implications]
\"twitter post\" = [\"twitter\", \"screenshot\"]
\"twitter\" = [\"social media\"]
",
        );
        let r = v.rename("twitter post", "social media"); // collapse the specific tag onto the general one
        assert!(r.changed);
        assert!(!v.implications.contains_key("twitter post"));
        assert_eq!(r.not_carried, vec!["twitter", "screenshot"]);
        assert!(!v.implications.contains_key("social media"));
        assert!(!v.implies("social media", "twitter"), "no cycle");
        let mut v = vocab(
            "[implications]
\"cat\" = [\"animal\"]
\"kitten\" = [\"cat\", \"young\"]
",
        );
        let r = v.rename("kitten", "animal"); // `animal` is the target of a rule, so it is a merge: nothing carried
        assert_eq!(r.not_carried, vec!["cat", "young"]);
        assert!(!v.implications.contains_key("animal"));
        assert_eq!(v.implications["cat"], vec!["animal"]);
        let mut v = vocab(
            "[implications]
\"kitten\" = [\"cat\", \"young\"]
",
        );
        let r = v.rename("kitten", "puppy"); // a plain rename: the rules follow
        assert!(r.not_carried.is_empty());
        assert_eq!(v.implications["puppy"], vec!["cat", "young"]);
    }
    #[test]
    fn a_vocab_that_does_not_parse_is_never_saved_over() {
        let dir = std::env::temp_dir().join(format!("memetag-vocab-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("vocab.toml");
        let text = "# hand edited\n[aliases]\n\"pg\" = \"peter griffin\"\n[implications]\n\"peter griffin\" = [\"family guy\"\n";
        std::fs::write(&p, text).unwrap();
        let mut v = Vocab::load(&p);
        assert!(v.broken.is_some());
        assert!(v.aliases.is_empty());
        let err = v.save().unwrap_err();
        assert!(err.contains("did not parse"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            text,
            "the file is untouched"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn stale_drafts_are_refused_but_repeated_saves_work() {
        let dir = std::env::temp_dir().join(format!("memetag-vocab-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("vocab.toml");
        let mut first = Vocab::load(&path);
        let mut stale = first.clone();
        first.add_rule("cat", "animal");
        first.save().unwrap();
        first.add_rule("dog", "animal");
        first.save().unwrap();
        stale.add_rule("bird", "animal");
        assert!(stale.save().unwrap_err().contains("Rules changed"));
        let stored = Vocab::load(&path);
        assert!(stored.implies("cat", "animal") && stored.implies("dog", "animal"));
        assert!(!stored.implies("bird", "animal"));
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn save_keeps_header_and_sorts() {
        let dir = std::env::temp_dir().join(format!("memetag-vocab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("vocab.toml");
        std::fs::write(&p, "# keep me\n\n[aliases]\n\"lol\" = \"reaction:laugh\"\n[implications]\n\"b\" = [\"z\", \"y\"]\n\"a\" = [\"b\"]\n").unwrap();
        let mut v = Vocab::load(&p);
        assert!(v.implies("a", "z"));
        assert!(!v.implies("z", "a"));
        assert!(!v.implies("a", "a"));
        v.add_rule("twitter post", "twitter");
        v.remove_rule("b", "y");
        v.save().unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.starts_with("# keep me\n\n[aliases]\n"), "{s}");
        let (a, b) = (
            s.find("a = [\"b\"]").expect(&s),
            s.find("b = [\"z\"]").expect(&s),
        );
        assert!(a < b, "{s}");
        assert!(s.contains("\"twitter post\" = [\"twitter\"]"), "{s}");
        assert!(!s.contains("\"y\""), "{s}");
        let again = Vocab::load(&p);
        assert_eq!(
            again.rules(),
            vec![
                ("a".into(), "b".into()),
                ("b".into(), "z".into()),
                ("twitter post".into(), "twitter".into())
            ]
        );
        assert_eq!(again.canon("LOL"), "reaction:laugh");
        std::fs::remove_dir_all(&dir).ok();
    }
}
