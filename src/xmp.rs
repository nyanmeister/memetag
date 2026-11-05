//! XMP packets: read tags/fields out of one, and MERGE ours into an existing one without
//! disturbing anything foreign (titles, ratings, digiKam lists, Lightroom state, …).
//! Only `dc:subject` and the `meme:*` properties are ours; every other byte is copied through.
use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::{QName, ResolveResult};
use quick_xml::reader::NsReader;
use quick_xml::writer::Writer;
use std::borrow::Cow;
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

/// Where a prefix of ours can be declared inside the `rdf:Description` we write into.
#[derive(Clone, Copy, PartialEq)]
enum Decl {
    /// already bound to the URI we want, by this element or an ancestor
    Have,
    /// unbound, or bound to one of our legacy URIs: declare (or redeclare) it on the Description
    Add,
    /// bound to somebody else's URI: declare ours on each property instead
    Inline,
}

fn decl_for(rd: &NsReader<&[u8]>, prefix: &str, want: &str) -> Decl {
    let probe = format!("{prefix}:x");
    match rd.resolver().resolve_element(QName(&probe)).0 {
        ResolveResult::Bound(ns) if ns.as_ref() == want => Decl::Have,
        ResolveResult::Bound(ns) if is_ours(ns.as_ref()) => Decl::Add,
        ResolveResult::Bound(_) => Decl::Inline,
        _ => Decl::Add,
    }
}

/// Line breaks and indentation copied from the packet we write into, so a one-line packet stays
/// one line and an exiftool packet keeps its one-space steps. `close` is the Description's own
/// indentation: it precedes `</rdf:Description>` and, plus one step, each property.
struct Layout {
    close: String,
}
impl Layout {
    /// From the whitespace before `<rdf:Description`; memetag 0.3.1 left the closing tag at column 0,
    /// so the opening indentation is the one to trust.
    fn from_ws(ws: Option<&str>) -> Layout {
        let close = ws
            .and_then(|w| w.rfind('\n').map(|i| w[i..].to_string()))
            .unwrap_or_default();
        Layout { close }
    }
    /// A child's indentation `level` steps below the Description.
    fn at(&self, level: usize) -> String {
        if self.close.is_empty() {
            String::new()
        } else {
            format!("{}{}", self.close, " ".repeat(level))
        }
    }
}

/// Our properties, indented one step below the Description, ending with the Description's own
/// closing indentation.
fn fragment(
    rdf: &str,
    lay: &Layout,
    dc: Decl,
    meme: Decl,
    tags: &[String],
    fields: &BTreeMap<String, String>,
) -> String {
    let (p1, p2, p3) = (lay.at(1), lay.at(2), lay.at(3));
    let mut s = String::new();
    if !tags.is_empty() {
        let d = if dc == Decl::Inline {
            format!(" xmlns:dc=\"{DC}\"")
        } else {
            String::new()
        };
        s.push_str(&format!("{p1}<dc:subject{d}>{p2}<{rdf}:Bag>"));
        for t in tags {
            s.push_str(&format!("{p3}<{rdf}:li>{}</{rdf}:li>", esc(t)));
        }
        s.push_str(&format!("{p2}</{rdf}:Bag>{p1}</dc:subject>"));
    }
    let d = if meme == Decl::Inline {
        format!(" xmlns:meme=\"{}\"", meme_ns())
    } else {
        String::new()
    };
    for (k, v) in fields {
        s.push_str(&format!("{p1}<meme:{k}{d}>{}</meme:{k}>", esc(v)));
    }
    s.push_str(&lay.close);
    s
}

/// The `xmlns:dc` / `xmlns:meme` declarations a Description start tag needs for `Decl::Add`.
fn declarations(dc: Decl, meme: Decl) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if dc == Decl::Add {
        v.push(("xmlns:dc".to_string(), DC.to_string()));
    }
    if meme == Decl::Add {
        v.push(("xmlns:meme".to_string(), meme_ns().to_string()));
    }
    v
}

/// A Description start tag without our attribute-form properties (and without a legacy
/// `xmlns:meme` when we redeclare it), plus the declarations we add. `None` when nothing changes,
/// so a foreign tag is copied byte for byte. Values are already escaped and are kept as they are.
fn rebuilt_description(
    rd: &NsReader<&[u8]>,
    e: &BytesStart,
    dc: Decl,
    meme: Decl,
) -> Option<BytesStart<'static>> {
    let mut changed = false;
    let mut keep: Vec<(String, String)> = Vec::new();
    for a in e.attributes().flatten() {
        let key = a.key.as_ref();
        let (ar, _) = rd.resolver().resolve_attribute(a.key);
        let drop = is_ours(ns_of(&ar)) || (key == "xmlns:meme" && meme == Decl::Add);
        if drop {
            changed = true;
        } else {
            keep.push((key.to_string(), a.value.to_string()));
        }
    }
    let add = declarations(dc, meme);
    if !changed && add.is_empty() {
        return None;
    }
    let mut ne = BytesStart::new(e.name().as_ref().to_string());
    for (k, v) in keep.iter().chain(add.iter()) {
        ne.push_attribute(Attribute {
            key: QName(k),
            value: Cow::Borrowed(v.as_str()),
        });
    }
    Some(ne.into_owned())
}

/// No padding block: memetag rewrites the container on every edit, so the 2-4 KB of spaces the
/// XMP spec suggests for in-place editors only made every file bigger (asked 2026-10-09).
fn fresh(tags: &[String], fields: &BTreeMap<String, String>) -> String {
    let lay = Layout {
        close: "\n  ".into(),
    };
    format!(
        "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"memetag {}\">\n \
<rdf:RDF xmlns:rdf=\"{RDF}\">\n  \
<rdf:Description rdf:about=\"\" xmlns:dc=\"{DC}\" xmlns:meme=\"{}\">{}\
</rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>",
        env!("CARGO_PKG_VERSION"),
        meme_ns(),
        fragment("rdf", &lay, Decl::Have, Decl::Have, tags, fields)
    )
}

fn put(w: &mut Writer<Vec<u8>>, ev: Event) -> Result<(), String> {
    w.write_event(ev).map_err(|e| e.to_string())
}
fn put_text(w: &mut Writer<Vec<u8>>, s: String) -> Result<(), String> {
    put(w, Event::Text(BytesText::from_escaped(s)))
}

/// Merge our properties into `existing` (or build a fresh packet). Everything not ours is copied
/// event-for-event, with two exceptions: whitespace that only indented a property we remove goes
/// with it (otherwise every retag left a blank line), and whitespace outside the root element,
/// which is where padding lives, shrinks to one line break.
pub fn merge(
    existing: Option<&str>,
    tags: &[String],
    fields: &BTreeMap<String, String>,
) -> Result<String, String> {
    let Some(src) = existing.filter(|s| s.contains("RDF")) else {
        return Ok(fresh(tags, fields));
    };
    let mut rd = NsReader::from_str(src);
    let mut w = Writer::new(Vec::with_capacity(src.len() + 1024));
    let (mut depth, mut skip_from, mut desc_depth, mut rdf_depth): (
        usize,
        Option<usize>,
        Option<usize>,
        Option<usize>,
    ) = (0, None, None, None);
    let (mut injected, mut rdf_prefix) = (false, String::from("rdf"));
    // how our prefixes are declared in the Description we inject into
    let (mut dc_decl, mut meme_decl) = (Decl::Add, Decl::Add);
    let (mut desc_lay, mut rdf_lay) = (Layout::from_ws(None), Layout::from_ws(None));
    // whitespace-only text not yet written: dropped if the next thing is a property we remove
    let mut pending: Option<String> = None;
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
                    pending = None;
                    continue;
                }
                let ws = pending.take();
                if let Some(ws) = &ws {
                    put_text(&mut w, ws.clone())?;
                }
                if ns == RDF && local == "RDF" {
                    rdf_depth = Some(depth);
                    rdf_prefix = prefix_of(&e);
                    rdf_lay = Layout::from_ws(ws.as_deref());
                }
                if ns == RDF && local == "Description" && !injected && desc_depth.is_none() {
                    desc_depth = Some(depth);
                    rdf_prefix = prefix_of(&e);
                    desc_lay = Layout::from_ws(ws.as_deref());
                    dc_decl = decl_for(&rd, "dc", DC);
                    meme_decl = decl_for(&rd, "meme", meme_ns());
                    if let Some(ne) = rebuilt_description(&rd, &e, dc_decl, meme_decl) {
                        put(&mut w, Event::Start(ne))?;
                        continue;
                    }
                }
                put(&mut w, Event::Start(e))?;
            }
            Event::Empty(e) => {
                if skip_from.is_some() {
                    continue;
                }
                let ns = ns_of(&res);
                if (ns == DC && e.local_name().as_ref() == "subject") || is_ours(ns) {
                    pending = None;
                    continue;
                }
                let ws = pending.take();
                if let Some(ws) = &ws {
                    put_text(&mut w, ws.clone())?;
                }
                if ns == RDF
                    && e.local_name().as_ref() == "Description"
                    && !injected
                    && desc_depth.is_none()
                {
                    // an empty Description: expand it and inject inside
                    let name = e.name().as_ref().to_string();
                    rdf_prefix = prefix_of(&e);
                    let (dc, meme) = (decl_for(&rd, "dc", DC), decl_for(&rd, "meme", meme_ns()));
                    let start = rebuilt_description(&rd, &e, dc, meme)
                        .unwrap_or_else(|| e.clone().into_owned());
                    put(&mut w, Event::Start(start))?;
                    let lay = Layout::from_ws(ws.as_deref());
                    put_text(&mut w, fragment(&rdf_prefix, &lay, dc, meme, tags, fields))?;
                    put(&mut w, Event::End(BytesEnd::new(name)))?;
                    injected = true;
                    continue;
                }
                put(&mut w, Event::Empty(e))?;
            }
            Event::End(e) => {
                if let Some(d) = skip_from {
                    if d == depth {
                        skip_from = None;
                    }
                    depth -= 1;
                    continue;
                }
                let ws = pending.take();
                if !injected && desc_depth == Some(depth) {
                    put_text(
                        &mut w,
                        fragment(&rdf_prefix, &desc_lay, dc_decl, meme_decl, tags, fields),
                    )?;
                    injected = true;
                } else if !injected && rdf_depth == Some(depth) {
                    // no Description at all: write one, declared on itself, in the RDF's own style
                    // (a root RDF has no indentation of its own; the closing tag's serves then)
                    let (dc, meme) = (decl_for(&rd, "dc", DC), decl_for(&rd, "meme", meme_ns()));
                    if rdf_lay.close.is_empty() {
                        rdf_lay = Layout::from_ws(ws.as_deref());
                    }
                    let lay = Layout {
                        close: rdf_lay.at(1),
                    };
                    let decls: String = declarations(dc, meme)
                        .iter()
                        .map(|(k, v)| format!(" {k}=\"{v}\""))
                        .collect();
                    put_text(
                        &mut w,
                        format!(
                            "{open}<{p}:Description {p}:about=\"\"{decls}>{}</{p}:Description>{close}",
                            fragment(&rdf_prefix, &lay, dc, meme, tags, fields),
                            p = rdf_prefix,
                            open = lay.close,
                            close = rdf_lay.close,
                        ),
                    )?;
                    injected = true;
                } else if let Some(ws) = ws {
                    put_text(&mut w, ws)?;
                }
                depth -= 1;
                put(&mut w, Event::End(e))?;
            }
            Event::Text(t) if skip_from.is_none() && t.trim().is_empty() => {
                let ws = if depth == 0 {
                    "\n".to_string()
                } else {
                    t.to_string()
                };
                match &mut pending {
                    Some(p) => p.push_str(&ws),
                    None => pending = Some(ws),
                }
            }
            Event::Eof => break,
            other => {
                if skip_from.is_none() {
                    if let Some(ws) = pending.take() {
                        put_text(&mut w, ws)?;
                    }
                    put(&mut w, other)?;
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
        // the legacy declaration on rdf:Description is replaced by the write namespace, once, and the
        // new property relies on it
        assert!(out.contains("<meme:id>new</meme:id>"), "{out}");
        assert!(
            out.contains(&format!("xmlns:meme=\"{DEFAULT_MEME}\""))
                && !out.contains("legacy.example"),
            "{out}"
        );
        assert!(!out.contains("<meme:origMtime"), "{out}");
        let p = read(&out).unwrap();
        assert_eq!(p.tags, vec!["a & b", "c"]);
        assert_eq!(p.fields.get("id").unwrap(), "new");
        assert!(!p.fields.contains_key("origMtime"));
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
#[cfg(test)]
mod layout_tests {
    //! Expected packets here are written by hand, not read off `fresh`: a reader of the files found the XML
    //! "full of junky whitespace" (asked 2026-10-09), a blank line per retag plus 2.6 KB of padding.
    use super::*;
    const PAD: &str =
        "                                                                                \n";
    fn f(id: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("id".to_string(), id.to_string())])
    }
    fn expected(xmptk: &str) -> String {
        format!(
            "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"{xmptk}\">\n \
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:meme=\"{DEFAULT_MEME}\">\n   \
<dc:subject>\n    <rdf:Bag>\n     <rdf:li>a</rdf:li>\n     <rdf:li>b &amp; c</rdf:li>\n    </rdf:Bag>\n   </dc:subject>\n   \
<meme:id>x</meme:id>\n  \
</rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n<?xpacket end=\"w\"?>"
        )
    }
    #[test]
    fn fresh_packet_has_no_padding_and_declares_namespaces_once() {
        let tags = ["a".to_string(), "b & c".to_string()];
        assert_eq!(
            fresh(&tags, &f("x")),
            expected(concat!("memetag ", env!("CARGO_PKG_VERSION")))
        );
    }
    #[test]
    fn retag_of_an_old_packet_removes_blank_lines_padding_and_repeated_declarations() {
        // what memetag 0.3.1 left after a retag: whitespace where the old properties stood, the
        // closing tag at column 0, a declaration on every property, 32 padding lines
        let old = format!(
            "<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"memetag 0.1\">\n \
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
<rdf:Description rdf:about=\"\">\n   \n   \n  \n   \
<dc:subject xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n    <rdf:Bag>\n     <rdf:li>old</rdf:li>\n    </rdf:Bag>\n   </dc:subject>\n   \
<meme:id xmlns:meme=\"{DEFAULT_MEME}\">old</meme:id>\n   \
<meme:text xmlns:meme=\"{DEFAULT_MEME}\">two\nlines</meme:text>\n\
</rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n{}<?xpacket end=\"w\"?>",
            PAD.repeat(32)
        );
        let tags = ["a".to_string(), "b & c".to_string()];
        let out = merge(Some(&old), &tags, &f("x")).unwrap();
        assert_eq!(out, expected("memetag 0.1"));
        // and a second pass changes nothing
        assert_eq!(merge(Some(&out), &tags, &f("x")).unwrap(), out);
    }
    #[test]
    fn foreign_packet_keeps_its_own_indentation_and_properties() {
        // exiftool's one-space style, with a title we must not touch
        let old = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Image::ExifTool 12.76\">\n\
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n \
<rdf:Description rdf:about=\"\"\n  xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\n  \
<dc:subject>\n   <rdf:Bag>\n    <rdf:li>theirs</rdf:li>\n   </rdf:Bag>\n  </dc:subject>\n  \
<dc:title>\n   <rdf:Alt>\n    <rdf:li xml:lang=\"x-default\">T &amp; U</rdf:li>\n   </rdf:Alt>\n  </dc:title>\n \
</rdf:Description>\n</rdf:RDF>\n</x:xmpmeta>";
        let want = format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\" x:xmptk=\"Image::ExifTool 12.76\">\n\
<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n \
<rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:meme=\"{DEFAULT_MEME}\">\n  \
<dc:title>\n   <rdf:Alt>\n    <rdf:li xml:lang=\"x-default\">T &amp; U</rdf:li>\n   </rdf:Alt>\n  </dc:title>\n  \
<dc:subject>\n   <rdf:Bag>\n    <rdf:li>mine</rdf:li>\n   </rdf:Bag>\n  </dc:subject>\n  \
<meme:id>1</meme:id>\n \
</rdf:Description>\n</rdf:RDF>\n</x:xmpmeta>"
        );
        assert_eq!(merge(Some(old), &["mine".into()], &f("1")).unwrap(), want);
    }
    #[test]
    fn compact_packet_stays_compact_and_taken_prefixes_are_declared_inline() {
        // `meme` and `dc` both belong to someone else here, so ours go on the property itself
        let old = format!(
            "<r:RDF xmlns:r=\"{RDF}\"><r:Description r:about=\"\" xmlns:meme=\"urn:theirs\" xmlns:dc=\"urn:notdc\" dc:x=\"1 &amp; 2\">\
<meme:id>theirs</meme:id><dc:x>keep</dc:x></r:Description></r:RDF>"
        );
        let want = format!(
            "<r:RDF xmlns:r=\"{RDF}\"><r:Description r:about=\"\" xmlns:meme=\"urn:theirs\" xmlns:dc=\"urn:notdc\" dc:x=\"1 &amp; 2\">\
<meme:id>theirs</meme:id><dc:x>keep</dc:x>\
<dc:subject xmlns:dc=\"{DC}\"><r:Bag><r:li>t</r:li></r:Bag></dc:subject>\
<meme:id xmlns:meme=\"{DEFAULT_MEME}\">ours</meme:id></r:Description></r:RDF>"
        );
        let out = merge(Some(&old), &["t".into()], &f("ours")).unwrap();
        assert_eq!(out, want);
        let p = read(&out).unwrap();
        assert_eq!(
            (p.tags, p.fields.get("id").map(String::as_str)),
            (vec!["t".to_string()], Some("ours"))
        );
    }
    #[test]
    fn rebuilt_description_keeps_foreign_attribute_values_escaped_once() {
        // dropping an attribute-form property rewrites the start tag; the values beside it are already escaped
        let old = format!(
            "<rdf:RDF xmlns:rdf=\"{RDF}\"><rdf:Description rdf:about=\"\" xmlns:meme=\"{DEFAULT_MEME}\" xmlns:dc=\"{DC}\" meme:id=\"old\" dc:title=\"T &amp; U\"/></rdf:RDF>"
        );
        let want = format!(
            "<rdf:RDF xmlns:rdf=\"{RDF}\"><rdf:Description rdf:about=\"\" xmlns:meme=\"{DEFAULT_MEME}\" xmlns:dc=\"{DC}\" dc:title=\"T &amp; U\"><meme:id>new</meme:id></rdf:Description></rdf:RDF>"
        );
        assert_eq!(merge(Some(&old), &[], &f("new")).unwrap(), want);
    }
    #[test]
    fn packet_without_description_gets_one_in_its_own_style() {
        let old = format!("<rdf:RDF xmlns:rdf=\"{RDF}\">\n</rdf:RDF>");
        let want = format!(
            "<rdf:RDF xmlns:rdf=\"{RDF}\">\n <rdf:Description rdf:about=\"\" xmlns:dc=\"{DC}\" xmlns:meme=\"{DEFAULT_MEME}\">\n  <meme:id>n</meme:id>\n </rdf:Description>\n</rdf:RDF>"
        );
        assert_eq!(merge(Some(&old), &[], &f("n")).unwrap(), want);
    }
}
