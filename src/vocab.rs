//! Aliases and implications, derpibooru style, from ~/.config/memetag/vocab.toml:
//!   [aliases]       "thisisfine" = "this is fine"          (query and file tags are canonicalised)
//!   [implications]  "character:reimu" = ["series:touhou"]  (added as derived tags at index time)
//!   [suggestions]   "catgirl" = ["tail", "kitty paw hand"]   (offered in the editor; accepted → the tag,
//!                                                            denied → `not:<tag>`, so "has none" is data too)
//!   [categories]    character = ["hatsune miku", "soyjak"]    (what kind of tag; the editor groups and tints
//!                   series = ["vocaloid"]                       by it; held per tag in memory, grouped in the file)
//!   [colors]        scene = "#2b4b70"                          (the tint a category's chips wear; the built-in
//!                                                               categories have one without an entry here)
//!
//! Relational tags (asked 2026-10-31): `modifier<target>` — `chibi<lain iwakura>`, `fumo<reimu hakurei>`,
//! `tiny<x>`, `referenced<spongebob>`, `visible<x>` — say the modifier applies to that one target, so a character
//! holding a plush of herself is `reimu hakurei` and `fumo<reimu hakurei>`. No nesting. The tag expands to its
//! two parts as implied tags, so a plain search for either still finds the file, and it sits in the target's
//! category. It is written to the file as the one string; XML escaping takes care of the brackets.
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Deserialize, serde::Serialize, Default, Debug, Clone)]
pub struct Vocab {
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub implications: HashMap<String, Vec<String>>,
    /// Tags that often go with a tag, offered per file in the editor (asked 2026-10-31). Unlike an implication
    /// nothing is derived: the owner accepts or denies each one on the file.
    #[serde(default)]
    pub suggestions: HashMap<String, Vec<String>>,
    /// tag → category (character, series, artist, …): what kind of tag it is, an attribute of the tag, never part
    /// of its name (asked 2026-10-31, mockup C+A). In the file the table is grouped the other way round,
    /// `category = ["tag", …]`, which is the shape a person edits; `de_categories` turns it into this map.
    #[serde(
        default,
        deserialize_with = "de_categories",
        serialize_with = "ser_categories"
    )]
    pub categories: HashMap<String, String>,
    /// category → "#rrggbb": the tint its chips wear (asked 2026-11-05: "how do I change the color of a category
    /// I've added?"). A category of the owner's own exists through this table even before a tag is in it; a
    /// built-in one has a tint of its own (`default_color`) and an entry here only once it was changed.
    #[serde(
        default,
        deserialize_with = "de_colors",
        skip_serializing_if = "HashMap::is_empty"
    )]
    pub colors: HashMap<String, String>,
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
            // Left out while empty, so a vocabulary without suggestions fingerprints exactly as it did before the
            // table existed (0.3.5): no machine sees a phantom change on the first sync after an update.
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            suggestions: BTreeMap<&'a str, Vec<&'a str>>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            categories: BTreeMap<&'a str, Vec<&'a str>>,
            #[serde(skip_serializing_if = "BTreeMap::is_empty")]
            colors: BTreeMap<&'a str, &'a str>,
        }
        fn sorted_lists(m: &HashMap<String, Vec<String>>) -> BTreeMap<&str, Vec<&str>> {
            m.iter()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| {
                    let mut v: Vec<&str> = v.iter().map(String::as_str).collect();
                    v.sort_unstable();
                    v.dedup();
                    (k.as_str(), v)
                })
                .collect()
        }
        let sorted = Sorted {
            aliases: self
                .aliases
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
            implications: sorted_lists(&self.implications),
            suggestions: sorted_lists(&self.suggestions),
            categories: group_categories(&self.categories),
            colors: self
                .colors
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
        };
        toml::to_string(&sorted).unwrap_or_default()
    }
    /// Trimmed, lowercased in every alphabet (as the text search folds case), then through the aliases; the alias
    /// target is normalised the same way, so a hand-written `"pg" = "Peter Griffin"` cannot put two spellings of one
    /// tag into a file (review, 2026-09-22). No hand-written tag in the collection had a non-ASCII letter that day.
    pub fn canon(&self, tag: &str) -> String {
        let t = tag.trim().to_lowercase();
        if let Some((m, target)) = relation(&t) {
            // each part through the aliases on its own; the shape stays
            return format!("{}<{}>", self.canon(m), self.canon(target));
        }
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
            // a relational tag implies its two parts, and they imply what they imply
            if let Some((m, target)) = relation(&t) {
                for part in [m, target] {
                    if out.insert(part.to_string()) {
                        queue.push(part.to_string());
                    }
                }
            }
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
    /// Every suggestion rule as (tag, suggested) pairs, sorted, for listing.
    pub fn suggestion_rules(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .suggestions
            .iter()
            .flat_map(|(k, vs)| vs.iter().map(move |v| (k.clone(), v.clone())))
            .collect();
        v.sort();
        v.dedup();
        v
    }
    pub fn suggests_directly(&self, from: &str, to: &str) -> bool {
        self.suggestions
            .get(from)
            .is_some_and(|v| v.iter().any(|x| x == to))
    }
    pub fn add_suggestion(&mut self, from: &str, to: &str) {
        let e = self.suggestions.entry(from.to_string()).or_default();
        if !e.iter().any(|x| x == to) {
            e.push(to.to_string());
        }
    }
    pub fn remove_suggestion(&mut self, from: &str, to: &str) {
        if let Some(e) = self.suggestions.get_mut(from) {
            e.retain(|x| x != to);
            if e.is_empty() {
                self.suggestions.remove(from);
            }
        }
    }
    /// What kind of tag this is. The table first; then the prefixes that already say it: `artist:` is an artist,
    /// `oc:` a character, and a denial (`not:x`) belongs with x, so it sits in x's row.
    pub fn category_of(&self, tag: &str) -> Option<String> {
        if let Some(c) = self.categories.get(tag) {
            return Some(c.clone());
        }
        if let Some(rest) = tag.strip_prefix(NOT) {
            return self.category_of(rest);
        }
        if let Some((_, target)) = relation(tag) {
            return self.category_of(target); // chibi<lain iwakura> sits with lain
        }
        match tag.split_once(':').map(|(p, _)| p) {
            Some("artist") => Some("artist".into()),
            Some("oc") => Some("character".into()),
            _ => None,
        }
    }
    /// Set or clear a tag's category (lowercase, trimmed; an empty name clears).
    pub fn set_category(&mut self, tag: &str, category: Option<&str>) {
        let c = category
            .map(|c| c.trim().to_lowercase())
            .filter(|c| !c.is_empty());
        match c {
            Some(c) => {
                self.categories.insert(tag.to_string(), c);
            }
            None => {
                self.categories.remove(tag);
            }
        }
    }
    /// Every category in row order: the known ones first, then any other name in the file (a tag's category, or
    /// a tint's), alphabetically.
    pub fn category_order(&self) -> Vec<String> {
        let mut v: Vec<String> = CATEGORIES.iter().map(|s| s.to_string()).collect();
        let mut extra: Vec<&String> = self
            .categories
            .values()
            .chain(self.colors.keys())
            .filter(|c| !CATEGORIES.contains(&c.as_str()))
            .collect();
        extra.sort();
        extra.dedup();
        v.extend(extra.into_iter().cloned());
        v
    }
    /// The tint a category's chips wear: the owner's entry in `[colors]`, else the built-in one. `None` means the
    /// plain button fill (an unknown category nobody tinted yet, or "other").
    pub fn color_of(&self, category: &str) -> Option<[u8; 3]> {
        self.colors
            .get(category)
            .and_then(|hex| parse_hex(hex))
            .or_else(|| default_color(category))
    }
    /// Set a category's tint, or clear the owner's entry (`None`: a built-in category falls back to its own tint,
    /// another to the plain fill). The name is lowercased and trimmed like a category.
    pub fn set_color(&mut self, category: &str, color: Option<[u8; 3]>) {
        let c = category.trim().to_lowercase();
        if c.is_empty() || c == OTHER {
            return;
        }
        match color {
            Some(rgb) => {
                let same_as_default = default_color(&c) == Some(rgb);
                if same_as_default {
                    self.colors.remove(&c);
                } else {
                    self.colors.insert(c, hex(rgb));
                }
            }
            None => {
                self.colors.remove(&c);
            }
        }
    }
    /// Give a category a tint if it has none: the first of the spare tints no category wears yet. This is how a
    /// category of the owner's own comes to exist (the Tags card's Add), and how one made by giving a tag a new
    /// name gets a colour at once instead of the plain fill.
    pub fn ensure_color(&mut self, category: &str) -> bool {
        let c = category.trim().to_lowercase();
        if c.is_empty() || c == OTHER || self.color_of(&c).is_some() {
            return false;
        }
        let worn: Vec<[u8; 3]> = self
            .category_order()
            .iter()
            .filter_map(|n| self.color_of(n))
            .collect();
        let pick = SPARE_TINTS
            .iter()
            .find(|t| !worn.contains(t))
            .or_else(|| SPARE_TINTS.get(self.colors.len() % SPARE_TINTS.len()))
            .copied()
            .unwrap_or([70, 70, 70]);
        self.colors.insert(c, hex(pick));
        true
    }
    /// Rename a category: every tag in it moves to the new name, and so does its tint (a built-in's own tint is
    /// written down for the new name, since the new name has none of its own). Onto a category that exists the
    /// two merge, the existing one keeping its tint. Returns how many tags moved.
    pub fn rename_category(&mut self, from: &str, to: &str) -> usize {
        let to = to.trim().to_lowercase();
        if to.is_empty() || to == OTHER || to == from {
            return 0;
        }
        let mut moved = 0;
        for c in self.categories.values_mut() {
            if c == from {
                *c = to.clone();
                moved += 1;
            }
        }
        if self.color_of(&to).is_none() {
            if let Some(rgb) = self.color_of(from) {
                self.set_color(&to, Some(rgb));
            }
        }
        self.colors.remove(from);
        moved
    }
    /// Take a category away: its tags have none afterwards, and its tint entry goes. Returns how many tags lost it.
    pub fn remove_category(&mut self, category: &str) -> usize {
        let before = self.categories.len();
        self.categories.retain(|_, c| c != category);
        self.colors.remove(category);
        before - self.categories.len()
    }
    /// The denial of a suggested tag, written to the file when the owner says the picture has none of it:
    /// `tail` → `not:tail`. A namespace rather than "no tail", so the editor can tell a denial from a tag that
    /// happens to start with "no" ("no girls allowed"), and the search grammar's `-tag` stays exclusion.
    pub fn denied(tag: &str) -> String {
        format!("{NOT}{tag}")
    }
    /// What the editor offers for a file: every suggestion keyed on any of `tags` (give it the full set, implied
    /// and folder tags included, so a rule written at the broad tag reaches the narrow one), minus what the file
    /// already has and minus what was denied with a `not:` tag. Sorted; targets canonical.
    pub fn suggests(&self, tags: &BTreeSet<String>) -> Vec<String> {
        let mut out = BTreeSet::new();
        for t in tags {
            if let Some(list) = self.suggestions.get(t) {
                for s in list {
                    let s = self.canon(s);
                    if !s.is_empty()
                        && !s.starts_with(NOT)
                        && !tags.contains(&s)
                        && !tags.contains(&Self::denied(&s))
                    {
                        out.insert(s);
                    }
                }
            }
        }
        out.into_iter().collect()
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
        // Suggestions follow the rename too, keys and targets; nothing is derived from them, so there is no
        // cycle to guard against, only "A suggests A" to drop.
        if let Some(list) = self.suggestions.remove(from) {
            r.changed = true;
            let e = self.suggestions.entry(to.to_string()).or_default();
            for t in list {
                if t != to && !e.contains(&t) {
                    e.push(t);
                }
            }
        }
        for (key, list) in self.suggestions.iter_mut() {
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
        self.suggestions.retain(|_, list| !list.is_empty());
        // the category travels with the name; a merge onto a tag with a category of its own keeps that one
        if let Some(c) = self.categories.remove(from) {
            r.changed = true;
            self.categories.entry(to.to_string()).or_insert(c);
        }
        for target in self.aliases.values_mut() {
            if target == from {
                *target = to.to_string();
                r.changed = true;
            }
        }
        r
    }
}

/// The namespace of a denied suggestion.
pub const NOT: &str = "not:";

/// `modifier<target>` → (modifier, target); None for a plain tag. Exactly one `<`, closing at the very end, both
/// parts non-empty, nothing nested (asked 2026-10-31: no additional nesting). A denial of one (`not:chibi<x>`) is
/// not itself relational; strip `not:` first if that matters.
pub fn relation(tag: &str) -> Option<(&str, &str)> {
    let rest = tag.strip_suffix('>')?;
    let (m, target) = rest.split_once('<')?;
    let (m, target) = (m.trim(), target.trim());
    if m.is_empty() || target.is_empty() || target.contains('<') || target.contains('>') {
        return None;
    }
    Some((m, target))
}

/// The categories in the order the editor shows their rows; a category named in vocab.toml that is not here comes
/// after them, alphabetically, and tags with no category sit under "other" at the end.
pub const CATEGORIES: &[&str] = &[
    "character",
    "series",
    "artist",
    "subject",
    "commentary",
    "theme",
    "format",
    "action",
];
/// The row name for tags without a category.
pub const OTHER: &str = "other";

/// The tint each built-in category wears unless `[colors]` says otherwise (mockup 2026-10-31, option A of round
/// 2): dark, readable with light text, one hue each.
pub fn default_color(category: &str) -> Option<[u8; 3]> {
    Some(match category {
        "character" => [47, 90, 49],
        "series" => [74, 53, 104],
        "artist" => [106, 47, 47],
        "subject" => [43, 75, 112],
        "commentary" => [96, 72, 30],
        "theme" => [30, 90, 90],
        "format" => [106, 74, 30],
        "action" => [90, 47, 90],
        _ => return None,
    })
}
/// Tints for categories of the owner's own, in the order `ensure_color` hands them out: the same darkness as the
/// built-in ones, hues between theirs.
pub const SPARE_TINTS: &[[u8; 3]] = &[
    [43, 92, 78],
    [100, 60, 36],
    [60, 60, 108],
    [94, 40, 72],
    [36, 84, 100],
    [84, 84, 36],
    [70, 48, 100],
    [100, 48, 60],
];
/// "#rrggbb" (a leading "#" optional, either case) to a colour; anything else is `None` and the entry is ignored.
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 || !h.is_ascii() {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([p(0)?, p(2)?, p(4)?])
}
/// The one spelling `[colors]` is written in.
pub fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}
/// `[colors]` keys fold like category names, and a value that is not a colour is dropped rather than carried.
fn de_colors<'de, D: serde::Deserializer<'de>>(d: D) -> Result<HashMap<String, String>, D::Error> {
    let raw: BTreeMap<String, String> = BTreeMap::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(c, v)| parse_hex(&v).map(|rgb| (c.trim().to_lowercase(), hex(rgb))))
        .collect())
}

/// `[categories]` in the file is `category = ["tag", …]`; in memory it is tag → category. A tag listed under two
/// categories takes the one that sorts first, so the file reads back the same every time.
fn de_categories<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<HashMap<String, String>, D::Error> {
    let grouped: BTreeMap<String, Vec<String>> = BTreeMap::deserialize(d)?;
    // lowercase first, then sort, so "Series" and "series" are one category and "character" wins over it
    let mut groups: Vec<(String, Vec<String>)> = grouped
        .into_iter()
        .map(|(c, t)| (c.trim().to_lowercase(), t))
        .collect();
    groups.sort();
    let mut out = HashMap::new();
    for (cat, tags) in groups {
        for t in tags {
            out.entry(t.trim().to_lowercase())
                .or_insert_with(|| cat.clone());
        }
    }
    Ok(out)
}
fn ser_categories<S: serde::Serializer>(
    m: &HashMap<String, String>,
    s: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&group_categories(m), s)
}
/// category → sorted tags, categories sorted: the shape of the file and of the normalized body.
fn group_categories(m: &HashMap<String, String>) -> BTreeMap<&str, Vec<&str>> {
    let mut g: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (t, c) in m {
        g.entry(c.as_str()).or_default().push(t.as_str());
    }
    for v in g.values_mut() {
        v.sort_unstable();
    }
    g
}

#[cfg(test)]
mod tests {
    use super::*;
    fn vocab(src: &str) -> Vocab {
        toml::from_str(src).unwrap()
    }
    #[test]
    fn suggestions_reach_through_implied_tags_and_stop_at_a_denial() {
        // Expected by hand. catgirl ⇒ kemonomimi; catgirl suggests kitty paw hand; kemonomimi suggests tail;
        // succubus suggests horns, wings, tail.
        let v = vocab(
            "[implications]\n\"catgirl\" = [\"kemonomimi\"]\n[suggestions]\n\"catgirl\" = [\"kitty paw hand\"]\n\"kemonomimi\" = [\"tail\"]\n\"succubus\" = [\"horns\", \"wings\", \"tail\"]\n",
        );
        let full = |tags: &[&str]| v.expand(tags.iter().map(|t| t.to_string()));
        // a catgirl file: the kemonomimi rule reaches it through the implication; sorted
        assert_eq!(v.suggests(&full(&["catgirl"])), ["kitty paw hand", "tail"]);
        // denied tail: gone, even though two rules would offer it; the kitty paw hand stays
        assert_eq!(
            v.suggests(&full(&["catgirl", "not:tail"])),
            ["kitty paw hand"]
        );
        // everything accepted: nothing left to offer
        assert!(v
            .suggests(&full(&["catgirl", "tail", "kitty paw hand"]))
            .is_empty());
        // a succubus with horns already: the other two
        assert_eq!(v.suggests(&full(&["succubus", "horns"])), ["tail", "wings"]);
        // no rule keys on it: nothing
        assert!(v.suggests(&full(&["cat"])).is_empty());
        assert_eq!(Vocab::denied("tail"), "not:tail");
    }
    #[test]
    fn normalized_body_leaves_an_empty_suggestions_table_out() {
        // The body is the sync fingerprint: a vocabulary without suggestions must serialise exactly as it did
        // before the table existed, so no machine sees a phantom change after the update.
        let mut v =
            vocab("[aliases]\nlol = \"reaction:laugh\"\n[implications]\nreimu = [\"touhou\"]\n");
        assert_eq!(
            v.normalized_body(),
            "[aliases]\nlol = \"reaction:laugh\"\n\n[implications]\nreimu = [\"touhou\"]\n"
        );
        v.add_suggestion("reimu", "hakurei shrine");
        v.add_suggestion("reimu", "gohei");
        v.add_suggestion("reimu", "gohei"); // twice is once
        assert_eq!(
            v.normalized_body(),
            "[aliases]\nlol = \"reaction:laugh\"\n\n[implications]\nreimu = [\"touhou\"]\n\n[suggestions]\nreimu = [\"gohei\", \"hakurei shrine\"]\n"
        );
        // and it reads back
        let back: Vocab = toml::from_str(&v.normalized_body()).unwrap();
        assert_eq!(back.suggestion_rules(), v.suggestion_rules());
        v.remove_suggestion("reimu", "gohei");
        v.remove_suggestion("reimu", "hakurei shrine");
        assert!(
            v.suggestions.is_empty(),
            "an emptied list takes its key with it"
        );
    }
    #[test]
    fn categories_are_grouped_in_the_file_and_per_tag_in_memory() {
        let mut v = vocab(
            "[categories]\ncharacter = [\"hatsune miku\", \"kazuma kiryu\"]\nseries = [\"yakuza (game)\"]\nSeries = [\"hatsune miku\"]\n",
        );
        // Expected by hand: per tag; a tag under two categories takes the one sorting first ("Series" lowercases
        // to series, and "character" < "series", so miku is a character)
        assert_eq!(v.category_of("hatsune miku").as_deref(), Some("character"));
        assert_eq!(v.category_of("yakuza (game)").as_deref(), Some("series"));
        assert_eq!(v.category_of("weed"), None);
        // prefixes that already say it, and a denial sits with its tag
        assert_eq!(v.category_of("artist:stonetoss").as_deref(), Some("artist"));
        assert_eq!(v.category_of("oc:nyan").as_deref(), Some("character"));
        assert_eq!(
            v.category_of("not:hatsune miku").as_deref(),
            Some("character")
        );
        assert_eq!(v.category_of("not:weed"), None);
        // the body groups them back, sorted, after the other tables; empty → left out
        assert_eq!(
            v.normalized_body(),
            "[aliases]\n\n[implications]\n\n[categories]\ncharacter = [\"hatsune miku\", \"kazuma kiryu\"]\nseries = [\"yakuza (game)\"]\n"
        );
        let back: Vocab = toml::from_str(&v.normalized_body()).unwrap();
        assert_eq!(back.categories, v.categories);
        v.set_category("weed", Some(" Subject "));
        assert_eq!(v.category_of("weed").as_deref(), Some("subject"));
        v.set_category("weed", None);
        v.set_category("hatsune miku", Some(""));
        assert_eq!(v.category_of("hatsune miku"), None);
        v.set_category("kazuma kiryu", None);
        v.set_category("yakuza (game)", None);
        assert!(v.categories.is_empty());
        assert_eq!(v.normalized_body(), "[aliases]\n\n[implications]\n");
        // row order: the known list, then a name of the owner's own, alphabetically
        v.set_category("x", Some("zzz"));
        v.set_category("y", Some("mood"));
        let order = v.category_order();
        assert_eq!(&order[..8], CATEGORIES);
        assert_eq!(&order[8..], ["mood", "zzz"]);
        // rename carries it
        assert!(v.rename("x", "x2").changed);
        assert_eq!(v.category_of("x2").as_deref(), Some("zzz"));
    }
    #[test]
    fn relational_tags_expand_to_their_parts_and_sit_with_the_target() {
        // Expected by hand. reimu ⇒ touhou; fumo<reimu hakurei> implies fumo and reimu hakurei, and reimu hakurei
        // implies touhou; the file keeps the one relational string as its own tag.
        let v = vocab(
            "[aliases]\nreimu = \"reimu hakurei\"\n[implications]\n\"reimu hakurei\" = [\"touhou\"]\n[categories]\ncharacter = [\"reimu hakurei\"]\n",
        );
        assert_eq!(
            relation("fumo<reimu hakurei>"),
            Some(("fumo", "reimu hakurei"))
        );
        assert_eq!(relation("fumo< reimu >"), Some(("fumo", "reimu")));
        assert_eq!(relation("plain"), None);
        assert_eq!(relation("a<b<c>>"), None, "no nesting");
        assert_eq!(relation("<x>"), None);
        assert_eq!(relation("x<>"), None);
        assert_eq!(relation("a<b>c"), None, "the bracket closes at the end");
        // aliases apply to the parts; the shape stays
        assert_eq!(v.canon("Fumo<REIMU>"), "fumo<reimu hakurei>");
        let got = v.expand(["fumo<reimu>".to_string()]);
        let want: BTreeSet<String> = ["fumo<reimu hakurei>", "fumo", "reimu hakurei", "touhou"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(got, want);
        // the modifier's own category wins nothing: the tag sits with its target
        assert_eq!(
            v.category_of("fumo<reimu hakurei>").as_deref(),
            Some("character")
        );
        assert_eq!(
            v.category_of("not:fumo<reimu hakurei>").as_deref(),
            Some("character")
        );
        assert_eq!(v.category_of("fumo<weed>"), None);
    }
    #[test]
    fn rename_carries_suggestions_too() {
        let mut v = vocab(
            "[suggestions]\n\"peter griffon\" = [\"family guy\"]\n\"stewie\" = [\"peter griffon\"]\n",
        );
        assert!(v.rename("peter griffon", "peter griffin").changed);
        assert_eq!(
            v.suggestion_rules(),
            vec![
                ("peter griffin".to_string(), "family guy".to_string()),
                ("stewie".to_string(), "peter griffin".to_string())
            ]
        );
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
    #[test]
    fn colors_come_from_the_file_then_the_built_in_table() {
        let v = vocab(
            "[categories]\nscene = [\"beach\"]\nmood = [\"cosy\"]\n[colors]\nScene = \"#2B4B70\"\ncharacter = \"102030\"\nbad = \"#12345\"\nmood = \"blue\"\n",
        );
        // Expected by hand: the key folds like a category, either case of hex reads, a built-in's own tint is
        // overridden by the file, an unparseable value is dropped, and a category with neither has None
        assert_eq!(v.color_of("scene"), Some([0x2b, 0x4b, 0x70]));
        assert_eq!(v.color_of("character"), Some([0x10, 0x20, 0x30]));
        assert_eq!(v.color_of("series"), Some([74, 53, 104]));
        assert_eq!(v.color_of("mood"), None);
        assert_eq!(v.color_of("bad"), None);
        assert_eq!(v.color_of(OTHER), None);
        // the file is written in one spelling, sorted; a category that exists only by its tint is still a category
        assert!(
            v.normalized_body()
                .ends_with("[colors]\ncharacter = \"#102030\"\nscene = \"#2b4b70\"\n"),
            "{}",
            v.normalized_body()
        );
        let back: Vocab = toml::from_str(&v.normalized_body()).unwrap();
        assert_eq!(back.colors, v.colors);
        let only_tint = vocab("[colors]\nzzz = \"#000000\"\n");
        assert_eq!(
            only_tint.category_order().last().map(String::as_str),
            Some("zzz")
        );
    }
    #[test]
    fn set_color_drops_an_entry_equal_to_the_built_in_tint() {
        let mut v = vocab("");
        v.set_color("character", Some([1, 2, 3]));
        assert_eq!(
            v.colors.get("character").map(String::as_str),
            Some("#010203")
        );
        v.set_color("character", Some([47, 90, 49])); // its own tint: nothing to say in the file
        assert!(v.colors.is_empty());
        v.set_color("character", Some([1, 2, 3]));
        v.set_color("character", None);
        assert!(v.colors.is_empty());
        assert_eq!(v.color_of("character"), Some([47, 90, 49]));
        v.set_color(OTHER, Some([1, 2, 3])); // "other" is the absence of a category: never tinted
        v.set_color("  ", Some([1, 2, 3]));
        assert!(v.colors.is_empty());
    }
    #[test]
    fn ensure_color_hands_out_spare_tints_nobody_wears() {
        let mut v = vocab("[categories]\nscene = [\"beach\"]\n");
        assert!(v.ensure_color("scene"));
        assert_eq!(v.color_of("scene"), Some(SPARE_TINTS[0]));
        assert!(!v.ensure_color("scene"), "already tinted: nothing to do");
        assert!(!v.ensure_color("character"), "a built-in has its own");
        assert!(!v.ensure_color(OTHER));
        // the next category skips the tint scene wears, and one a built-in was changed to
        v.set_color("series", Some(SPARE_TINTS[1]));
        assert!(v.ensure_color("mood"));
        assert_eq!(v.color_of("mood"), Some(SPARE_TINTS[2]));
        assert_eq!(v.category_order()[8..], ["mood", "scene"]);
    }
    #[test]
    fn rename_category_moves_its_tags_and_its_tint() {
        let mut v = vocab("[categories]\nscene = [\"beach\", \"poolside\"]\nmood = [\"cosy\"]\n[colors]\nscene = \"#2b4b70\"\n");
        // onto a new name: tags and tint follow; the old name is gone
        assert_eq!(v.rename_category("scene", " Setting "), 2);
        assert_eq!(v.category_of("beach").as_deref(), Some("setting"));
        assert_eq!(v.color_of("setting"), Some([0x2b, 0x4b, 0x70]));
        assert!(!v.colors.contains_key("scene"));
        assert!(!v.category_order().contains(&"scene".to_string()));
        // onto an existing category: a merge; the existing one keeps its tint
        v.set_color("mood", Some([9, 9, 9]));
        assert_eq!(v.rename_category("setting", "mood"), 2);
        assert_eq!(v.category_of("poolside").as_deref(), Some("mood"));
        assert_eq!(v.color_of("mood"), Some([9, 9, 9]));
        assert!(!v.colors.contains_key("setting"));
        // a built-in renamed: the new name is written down with the built-in's own tint
        v.set_category("miku", Some("character"));
        assert_eq!(v.rename_category("character", "person"), 1);
        assert_eq!(v.color_of("person"), Some([47, 90, 49]));
        assert_eq!(v.colors.get("person").map(String::as_str), Some("#2f5a31"));
        // refused: empty, "other", or the same name
        assert_eq!(v.rename_category("mood", ""), 0);
        assert_eq!(v.rename_category("mood", OTHER), 0);
        assert_eq!(v.rename_category("mood", "mood"), 0);
        assert_eq!(v.category_of("cosy").as_deref(), Some("mood"));
    }
    #[test]
    fn remove_category_leaves_its_tags_without_one() {
        let mut v = vocab(
            "[categories]\nscene = [\"beach\"]\nmood = [\"cosy\"]\n[colors]\nscene = \"#2b4b70\"\n",
        );
        assert_eq!(v.remove_category("scene"), 1);
        assert_eq!(v.category_of("beach"), None);
        assert_eq!(v.category_of("cosy").as_deref(), Some("mood"));
        assert!(v.colors.is_empty());
        assert_eq!(v.remove_category("nothing"), 0);
    }
    #[test]
    fn hex_round_trips_and_rejects_the_rest() {
        assert_eq!(parse_hex("#2b4b70"), Some([0x2b, 0x4b, 0x70]));
        assert_eq!(parse_hex(" 2B4B70 "), Some([0x2b, 0x4b, 0x70]));
        assert_eq!(parse_hex("#2b4b7"), None);
        assert_eq!(parse_hex("#2b4b70ff"), None);
        assert_eq!(parse_hex("#gg4b70"), None);
        assert_eq!(
            parse_hex("#2b4b7\u{e9}"),
            None,
            "a non-ASCII byte must not slice a char in two"
        );
        assert_eq!(hex([0, 171, 255]), "#00abff");
    }
}
