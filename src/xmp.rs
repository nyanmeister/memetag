//! XMP packets: read tags/fields out of one, and MERGE ours into an existing one without
//! disturbing anything foreign (titles, ratings, digiKam lists, Lightroom state, …).
//! Only `dc:subject` and the `meme:*` properties are ours; every other byte is copied through.
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::writer::Writer;
use std::collections::BTreeMap;

pub const DC: &str = "http://purl.org/dc/elements/1.1/";
/// The namespace memetag writes its own properties under. Any URI works as long as every writer of a
/// collection uses the same one; `xmp_namespace` in config.toml (or `MEMETAG_XMP_NAMESPACE`) overrides it.
pub const DEFAULT_MEME: &str = "https://github.com/nyanmeister/memetag/ns/meme/1.0/";
static MEME_NS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
/// Namespaces earlier builds wrote: read as ours, replaced by `meme_ns()` on the next write of that file.
/// `legacy_xmp_namespaces = [...]` in config.toml, or `MEMETAG_LEGACY_XMP_NAMESPACES` (comma separated).
static LEGACY_NS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// The namespace new properties are written under.
pub fn meme_ns() -> &'static str {
    MEME_NS.get().map(String::as_str).unwrap_or(DEFAULT_MEME)
}

/// The configured legacy namespaces, for `doctor`.
pub fn legacy_ns() -> Option<&'static [String]> {
    LEGACY_NS.get().map(Vec::as_slice)
}

/// Is `ns` one of ours: the write namespace or a configured legacy one.
pub fn is_ours(ns: &str) -> bool {
    ns == meme_ns() || LEGACY_NS.get().is_some_and(|l| l.iter().any(|x| x == ns))
}

/// Pick the namespaces once per process, before any file is read: config.toml keys `xmp_namespace` and
/// `legacy_xmp_namespaces`, then the environment. A second call is ignored, so tests can set their own first.
pub fn init() {
    // a file that does not parse is reported by `cfg()`, which every command runs first; here it counts as empty
    let t = crate::paths::config_table().unwrap_or_default();
    let mut write = t
        .get("xmp_namespace")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let mut legacy: Vec<String> = t
        .get("legacy_xmp_namespaces")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if let Ok(v) = std::env::var("MEMETAG_XMP_NAMESPACE") {
        write = Some(v);
    }
    if let Ok(v) = std::env::var("MEMETAG_LEGACY_XMP_NAMESPACES") {
        legacy = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }
    configure(write, legacy);
}

/// `init` without the config file; the first call per process wins.
pub fn configure(write: Option<String>, legacy: Vec<String>) {
    let write = write
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let _ = MEME_NS.set(write.unwrap_or_else(|| DEFAULT_MEME.to_string()));
    let _ = LEGACY_NS.set(
        legacy
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect(),
    );
}
pub const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

#[derive(Default, Debug, Clone)]
pub struct Parsed {
    pub tags: Vec<String>,
    pub fields: BTreeMap<String, String>,
}

fn ns_of<'a>(r: &'a ResolveResult) -> &'a str {
    match r {
        ResolveResult::Bound(ns) => ns.as_ref(),
        _ => "",
    }
}
/// Escaped for XML; control characters XML 1.0 cannot carry at all (OCR output has produced form feeds and
/// escape bytes) are dropped, since a packet holding one is unreadable to every other tool and to `read` here.
fn esc(s: &str) -> String {
    let clean: String = s
        .chars()
        .filter(|c| !matches!(*c as u32, 0..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f))
        .collect();
    quick_xml::escape::escape(&clean).into_owned()
}
fn unesc(s: &str) -> String {
    quick_xml::escape::unescape(s)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

pub fn read(packet: &str) -> Result<Parsed, String> {
    let mut rd = NsReader::from_str(packet);
    let mut out = Parsed::default();
    let (mut in_subject, mut in_li, mut meme_field): (bool, bool, Option<String>) =
        (false, false, None);
    let mut buf = String::new();
    loop {
        let (res, ev) = rd
            .read_resolved_event()
            .map_err(|e| format!("xmp parse: {e}"))?;
        match ev {
            Event::Start(e) => {
                let ns = ns_of(&res);
                let local = e.local_name();
                let local = local.as_ref();
                if ns == DC && local == "subject" {
                    in_subject = true;
                } else if in_subject && ns == RDF && local == "li" {
                    in_li = true;
                    buf.clear();
                } else if is_ours(ns) {
                    meme_field = Some(local.to_string());
                    buf.clear();
                } else if ns == RDF && local == "Description" {
                    for a in e.attributes().flatten() {
                        let (ar, al) = rd.resolver().resolve_attribute(a.key);
                        if is_ours(ns_of(&ar)) {
                            out.fields.insert(al.as_ref().to_string(), unesc(&a.value));
                        }
                    }
                }
            }
            Event::Empty(e) => {
                let ns = ns_of(&res);
                if ns == RDF && e.local_name().as_ref() == "Description" {
                    for a in e.attributes().flatten() {
                        let (ar, al) = rd.resolver().resolve_attribute(a.key);
                        if is_ours(ns_of(&ar)) {
                            out.fields.insert(al.as_ref().to_string(), unesc(&a.value));
                        }
                    }
                }
            }
            Event::Text(t) => {
                if in_li || meme_field.is_some() {
                    buf.push_str(&t.into_inner());
                }
            }
            Event::CData(t) => {
                if in_li || meme_field.is_some() {
                    buf.push_str(&esc(t.as_ref()));
                }
            }
            Event::GeneralRef(r) => {
                if in_li || meme_field.is_some() {
                    buf.push('&');
                    buf.push_str(r.as_ref());
                    buf.push(';');
                }
            }
            Event::End(e) => {
                let ns = ns_of(&res);
                let local = e.local_name();
                let local = local.as_ref();
                if in_li && ns == RDF && local == "li" {
                    in_li = false;
                    let t = unesc(&buf).trim().to_string();
                    if !t.is_empty() {
                        out.tags.push(t);
                    }
                } else if in_subject && ns == DC && local == "subject" {
                    in_subject = false;
                } else if let Some(f) = meme_field.take() {
                    if is_ours(ns) {
                        out.fields.insert(f, unesc(&buf));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

fn fragment(rdf: &str, tags: &[String], fields: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    if !tags.is_empty() {
        s.push_str(&format!(
            "\n   <dc:subject xmlns:dc=\"{}\">\n    <{rdf}:Bag>\n",
            DC
        ));
        for t in tags {
            s.push_str(&format!("     <{rdf}:li>{}</{rdf}:li>\n", esc(t)));
        }
        s.push_str(&format!("    </{rdf}:Bag>\n   </dc:subject>"));
    }
    for (k, v) in fields {
        s.push_str(&format!(
            "\n   <meme:{k} xmlns:meme=\"{}\">{}</meme:{k}>",
            meme_ns(),
            esc(v)
        ));
    }
    s.push('\n');
    s
}

fn fresh(tags: &[String], fields: &BTreeMap<String, String>) -> String {
    let mut s = String::new();
    s.push_str("<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"memetag 0.1\">\n <rdf:RDF xmlns:rdf=\"");
    s.push_str(RDF);
    s.push_str("\">\n  <rdf:Description rdf:about=\"\">");
    s.push_str(&fragment("rdf", tags, fields));
    s.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n");
    for _ in 0..32 {
        s.push_str(
            "                                                                                \n",
        );
    }
    s.push_str("<?xpacket end=\"w\"?>");
    s
}

/// Merge our properties into `existing` (or build a fresh packet). Everything not ours is copied event-for-event.
pub fn merge(
    existing: Option<&str>,
    tags: &[String],
    fields: &BTreeMap<String, String>,
) -> Result<String, String> {
    let Some(src) = existing.filter(|s| s.contains("RDF")) else {
        return Ok(fresh(tags, fields));
    };
    let mut rd = NsReader::from_str(src);
    let mut w = Writer::new(Vec::with_capacity(src.len() + 2048));
    let (mut depth, mut skip_from, mut desc_depth, mut rdf_depth): (
        usize,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    ) = (0, None, None, None);
    let (mut injected, mut rdf_prefix) = (false, String::from("rdf"));
    let prefix_of = |e: &BytesStart| {
        e.name()
            .prefix()
            .map(|p| p.as_ref().to_string())
            .unwrap_or_default()
    };
    loop {
        let (res, ev) = rd
            .read_resolved_event()
            .map_err(|e| format!("xmp parse: {e}"))?;
        match ev {
            Event::Start(e) => {
                depth += 1;
                if skip_from.is_some() {
                    continue;
                }
                let ns = ns_of(&res);
                let local = e.local_name().as_ref().to_string();
                if (ns == DC && local == "subject") || is_ours(ns) {
                    skip_from = Some(depth);
                    continue;
                }
                if ns == RDF && local == "RDF" {
                    rdf_depth = Some(depth);
                    rdf_prefix = prefix_of(&e);
                }
                if ns == RDF && local == "Description" && !injected && desc_depth.is_none() {
                    desc_depth = Some(depth);
                    rdf_prefix = prefix_of(&e);
                    // drop attribute-form meme:* properties from this Description
                    let keep: Vec<(String, String)> = e
                        .attributes()
                        .flatten()
                        .filter(|a| {
                            let (ar, _) = rd.resolver().resolve_attribute(a.key);
                            !is_ours(ns_of(&ar))
                        })
                        .map(|a| (a.key.as_ref().to_string(), a.value.to_string()))
                        .collect();
                    if keep.len() != e.attributes().count() {
                        let mut ne = BytesStart::new(e.name().as_ref().to_string());
                        for (k, v) in &keep {
                            ne.push_attribute((k.as_str(), v.as_str()));
                        }
                        w.write_event(Event::Start(ne)).map_err(|e| e.to_string())?;
                        continue;
                    }
                }
                w.write_event(Event::Start(e)).map_err(|e| e.to_string())?;
            }
            Event::Empty(e) => {
                if skip_from.is_some() {
                    continue;
                }
                let ns = ns_of(&res);
                if (ns == DC && e.local_name().as_ref() == "subject") || is_ours(ns) {
                    continue;
                }
                if ns == RDF
                    && e.local_name().as_ref() == "Description"
                    && !injected
                    && desc_depth.is_none()
                {
                    // an empty Description: expand it and inject inside
                    let name = e.name().as_ref().to_string();
                    rdf_prefix = prefix_of(&e);
                    let keep: Vec<(String, String)> = e
                        .attributes()
                        .flatten()
                        .filter(|a| {
                            let (ar, _) = rd.resolver().resolve_attribute(a.key);
                            !is_ours(ns_of(&ar))
                        })
                        .map(|a| (a.key.as_ref().to_string(), a.value.to_string()))
                        .collect();
                    let mut ne = BytesStart::new(name.clone());
                    for (k, v) in &keep {
                        ne.push_attribute((k.as_str(), v.as_str()));
                    }
                    w.write_event(Event::Start(ne)).map_err(|e| e.to_string())?;
                    w.write_event(Event::Text(BytesText::from_escaped(fragment(
                        &rdf_prefix,
                        tags,
                        fields,
                    ))))
                    .map_err(|e| e.to_string())?;
                    w.write_event(Event::End(BytesEnd::new(name)))
                        .map_err(|e| e.to_string())?;
                    injected = true;
                    continue;
                }
                w.write_event(Event::Empty(e)).map_err(|e| e.to_string())?;
            }
            Event::End(e) => {
                if let Some(d) = skip_from {
                    if d == depth {
                        skip_from = None;
                    }
                    depth -= 1;
                    continue;
                }
                if !injected && desc_depth == Some(depth) {
                    w.write_event(Event::Text(BytesText::from_escaped(fragment(
                        &rdf_prefix,
                        tags,
                        fields,
                    ))))
                    .map_err(|e| e.to_string())?;
                    injected = true;
                }
                if !injected && rdf_depth == Some(depth) {
                    let frag = format!(
                        "\n  <{p}:Description {p}:about=\"\">{}  </{p}:Description>\n",
                        fragment(&rdf_prefix, tags, fields),
                        p = rdf_prefix
                    );
                    w.write_event(Event::Text(BytesText::from_escaped(frag)))
                        .map_err(|e| e.to_string())?;
                    injected = true;
                }
                depth -= 1;
                w.write_event(Event::End(e)).map_err(|e| e.to_string())?;
            }
            Event::Eof => break,
            other => {
                if skip_from.is_none() {
                    w.write_event(other).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    if !injected {
        return Err("xmp: no rdf:RDF element found; refusing to guess".into());
    }
    String::from_utf8(w.into_inner()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn merge_keeps_foreign_and_replaces_ours() {
        // the packet was written by an earlier build under a namespace this build only reads
        configure(None, vec!["https://legacy.example/ns/meme/1.0/".into()]);
        let old = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?><x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:meme="https://legacy.example/ns/meme/1.0/" meme:id="old"><dc:title><rdf:Alt><rdf:li xml:lang="x-default">keep me</rdf:li></rdf:Alt></dc:title><dc:subject><rdf:Bag><rdf:li>old tag</rdf:li></rdf:Bag></dc:subject><meme:origMtime>1</meme:origMtime></rdf:Description></rdf:RDF></x:xmpmeta><?xpacket end="w"?>"#;
        let mut f = BTreeMap::new();
        f.insert("id".to_string(), "new".to_string());
        let out = merge(
            Some(old),
            &["a &amp; b".to_string().replace("&amp;", "&"), "c".into()],
            &f,
        )
        .unwrap();
        assert!(out.contains("keep me"));
        assert!(!out.contains("old tag"));
        assert!(!out.contains("meme:id=\"old\""));
        assert!(out.contains("<meme:id xmlns:meme"));
        // the new property carries the write namespace; the old declaration on rdf:Description stays as an unused prefix
        assert!(
            out.contains(&format!("xmlns:meme=\"{DEFAULT_MEME}\">new")),
            "{out}"
        );
        assert!(!out.contains("<meme:origMtime"), "{out}");
        let p = read(&out).unwrap();
        assert_eq!(p.tags, vec!["a & b", "c"]);
        assert_eq!(p.fields.get("id").unwrap(), "new");
        assert!(p.fields.get("origMtime").is_none());
    }
    #[test]
    fn fresh_roundtrip() {
        let mut f = BTreeMap::new();
        f.insert("id".into(), "x".into());
        let p = read(&fresh(&["t<1>".into()], &f)).unwrap();
        assert_eq!(p.tags, vec!["t<1>"]);
    }
}
#[cfg(test)]
mod namespace_tests {
    use super::*;
    #[test]
    fn unknown_meme_namespace_is_foreign() {
        // same prefix, a namespace nobody configured: copied through, not read as ours
        let old = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:meme="https://other.example/ns/"><meme:id>theirs</meme:id></rdf:Description></rdf:RDF></x:xmpmeta>"#;
        assert!(read(old).unwrap().fields.is_empty());
        let mut f = BTreeMap::new();
        f.insert("id".to_string(), "ours".to_string());
        let out = merge(Some(old), &["t".into()], &f).unwrap();
        assert!(out.contains("<meme:id>theirs</meme:id>"), "{out}");
        assert_eq!(read(&out).unwrap().fields.get("id").unwrap(), "ours");
    }
    #[test]
    fn default_namespace_is_the_public_one() {
        assert!(meme_ns().starts_with("https://github.com/"));
        assert!(is_ours(meme_ns()));
        assert!(!is_ours("https://other.example/ns/"));
    }
}
