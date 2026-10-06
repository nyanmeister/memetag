//! Matroska and WebM. Tags live in a `Tags` element of our own at the end of the Segment: one `Tag` aimed at the
//! whole file (TargetTypeValue 50) holding the XMP packet in a `SimpleTag` named XMP, with the tag names mirrored
//! into KEYWORDS for other tools. Clusters, Cues, Info, Tracks and any foreign Tags are never moved or rewritten;
//! the SeekHead gets an entry for ours when the Void beside it has room, so ffprobe and players that follow the
//! SeekHead see the tags at open. A rewrite truncates our old element (it is last) and appends the new one, so a
//! file never accumulates padding. No mkvtoolnix is needed to write; mkvinfo and mkvpropedit are the oracle in the
//! shell checks recorded on the graph. Bytes after the Segment are kept as they are.
//!
//! Refused: an element of unknown size (a file still being streamed) — nothing can be skipped past it.

const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const TAGS: u32 = 0x1254_C367;
const TAG: u32 = 0x7373;
const TARGETS: u32 = 0x63C0;
const TARGET_TYPE_VALUE: u32 = 0x68CA;
const TARGET_TRACK_UID: u32 = 0x63C5;
const TARGET_EDITION_UID: u32 = 0x63C9;
const TARGET_CHAPTER_UID: u32 = 0x63C4;
const TARGET_ATTACHMENT_UID: u32 = 0x63C6;
const SIMPLE_TAG: u32 = 0x67C8;
const TAG_NAME: u32 = 0x45A3;
const TAG_LANGUAGE: u32 = 0x447A;
const TAG_STRING: u32 = 0x4487;
const VOID: u32 = 0xEC;
const CRC32: u32 = 0xBF;

/// The SimpleTag that carries the packet.
pub const XMP_NAME: &str = "XMP";
const KEYWORDS_NAME: &str = "KEYWORDS";
/// Matroska's value for "the whole file" in TargetTypeValue.
const WHOLE_FILE: u8 = 50;

// ---------- EBML primitives ----------

fn vint_len(first: u8) -> Result<usize, String> {
    if first == 0 {
        return Err("ebml: a length byte of zero".into());
    }
    Ok(first.leading_zeros() as usize + 1)
}

/// An element id with its marker bits, as the file writes it (1 to 4 bytes).
fn read_id(b: &[u8], at: usize) -> Result<(u32, usize), String> {
    let first = *b.get(at).ok_or("ebml: truncated element id")?;
    let n = vint_len(first)?;
    if n > 4 || at + n > b.len() {
        return Err(format!("ebml: bad element id at {at}"));
    }
    let v = b[at..at + n].iter().fold(0u32, |a, x| (a << 8) | *x as u32);
    Ok((v, n))
}

/// An element size (1 to 8 bytes, marker removed); `None` is the reserved all-ones "unknown".
fn read_size(b: &[u8], at: usize) -> Result<(Option<u64>, usize), String> {
    let first = *b.get(at).ok_or("ebml: truncated element size")?;
    let n = vint_len(first)?;
    if n > 8 || at + n > b.len() {
        return Err(format!("ebml: bad element size at {at}"));
    }
    let mut v = (first as u64) & ((0xFFu32 >> n) as u64);
    for x in &b[at + 1..at + n] {
        v = (v << 8) | *x as u64;
    }
    let unknown = (1u64 << (7 * n)) - 1;
    Ok((if v == unknown { None } else { Some(v) }, n))
}

fn encode_id(id: u32) -> Vec<u8> {
    let n = 4 - (id.leading_zeros() as usize / 8);
    id.to_be_bytes()[4 - n..].to_vec()
}

/// The narrowest width whose all-ones value stays free for "unknown".
fn size_width(n: u64) -> usize {
    (1..=8).find(|w| n < (1u64 << (7 * w)) - 1).unwrap_or(8)
}

fn encode_size(n: u64, width: usize) -> Vec<u8> {
    let mut v = n.to_be_bytes()[8 - width..].to_vec();
    v[0] |= 1 << (8 - width);
    v
}

fn element_with(id: u32, body: &[u8], width: usize) -> Vec<u8> {
    let mut v = encode_id(id);
    v.extend(encode_size(body.len() as u64, width));
    v.extend_from_slice(body);
    v
}

fn element(id: u32, body: &[u8]) -> Vec<u8> {
    element_with(id, body, size_width(body.len() as u64))
}

/// Big-endian without leading zero bytes, at least one byte.
fn uint(v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|x| *x != 0).unwrap_or(7);
    b[skip..].to_vec()
}

/// Exactly `len` bytes of padding (`len` >= 2).
fn void(len: usize) -> Vec<u8> {
    for w in 1..=8 {
        if let Some(data) = len.checked_sub(1 + w) {
            if (data as u64) < (1u64 << (7 * w)) - 1 {
                let mut v = vec![0xEC];
                v.extend(encode_size(data as u64, w));
                v.resize(len, 0);
                return v;
            }
        }
    }
    unreachable!("a void of {len} bytes")
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct El {
    id: u32,
    start: usize,
    data: usize,
    end: usize,
}

/// The elements tiling `start..end`, each with a known size.
fn children(b: &[u8], start: usize, end: usize) -> Result<Vec<El>, String> {
    let mut v = vec![];
    let mut at = start;
    while at < end {
        let (id, n) = read_id(b, at)?;
        let (size, m) = read_size(b, at + n)?;
        let data = at + n + m;
        let size = size.ok_or_else(|| {
            format!("matroska: element {id:#x} at {at} has an unknown size (a file still being streamed?); nothing can be placed after it")
        })?;
        let el_end = data
            .checked_add(size as usize)
            .filter(|e| *e <= end)
            .ok_or_else(|| format!("matroska: element {id:#x} at {at} runs past its parent"))?;
        v.push(El {
            id,
            start: at,
            data,
            end: el_end,
        });
        at = el_end;
    }
    Ok(v)
}

fn uint_of(b: &[u8], e: &El) -> u64 {
    b[e.data..e.end]
        .iter()
        .take(8)
        .fold(0u64, |a, x| (a << 8) | *x as u64)
}

fn string_of(b: &[u8], e: &El) -> String {
    String::from_utf8_lossy(&b[e.data..e.end])
        .trim_end_matches('\0')
        .to_string()
}

struct Layout {
    seg_size_at: usize,
    seg_size_len: usize,
    seg_data: usize,
    seg_end: usize,
    seg_known: bool,
    children: Vec<El>,
}

fn layout(b: &[u8]) -> Result<Layout, String> {
    let (id, n) = read_id(b, 0)?;
    if id != EBML_HEADER {
        return Err("matroska: no EBML header".into());
    }
    let (size, m) = read_size(b, n)?;
    let seg_at = n + m + size.ok_or("matroska: EBML header of unknown size")? as usize;
    let (id, n2) = read_id(b, seg_at)?;
    if id != SEGMENT {
        return Err("matroska: no Segment after the EBML header".into());
    }
    let (size, m2) = read_size(b, seg_at + n2)?;
    let seg_data = seg_at + n2 + m2;
    let (seg_end, seg_known) = match size {
        Some(s) => {
            let end = seg_data
                .checked_add(s as usize)
                .filter(|e| *e <= b.len())
                .ok_or("matroska: the Segment runs past the end of the file")?;
            (end, true)
        }
        None => (b.len(), false),
    };
    Ok(Layout {
        seg_size_at: seg_at + n2,
        seg_size_len: m2,
        seg_data,
        seg_end,
        seg_known,
        children: children(b, seg_data, seg_end)?,
    })
}

// ---------- our Tag ----------

fn simple_tag(name: &str, value: &str) -> Vec<u8> {
    let body = [
        element(TAG_NAME, name.as_bytes()),
        element(TAG_LANGUAGE, b"und"),
        element(TAG_STRING, value.as_bytes()),
    ]
    .concat();
    element(SIMPLE_TAG, &body)
}

fn build_tags(xmp: &str, keywords: &str) -> Vec<u8> {
    let mut tag = element(TARGETS, &element(TARGET_TYPE_VALUE, &[WHOLE_FILE]));
    tag.extend(simple_tag(XMP_NAME, xmp));
    if !keywords.is_empty() {
        tag.extend(simple_tag(KEYWORDS_NAME, keywords));
    }
    element(TAGS, &element(TAG, &tag))
}

/// Every Tag child of a Tags element, with the packet when the Tag is ours: aimed at the whole file (no track,
/// edition, chapter or attachment target) and carrying a SimpleTag named XMP.
fn tags_of(b: &[u8], tags: &El) -> Result<Vec<(El, Option<String>)>, String> {
    let mut v = vec![];
    for tag in children(b, tags.data, tags.end)?
        .into_iter()
        .filter(|e| e.id == TAG)
    {
        let mut whole_file = true;
        let mut xmp = None;
        for c in children(b, tag.data, tag.end)? {
            match c.id {
                TARGETS => {
                    for t in children(b, c.data, c.end)? {
                        let uid = matches!(
                            t.id,
                            TARGET_TRACK_UID
                                | TARGET_EDITION_UID
                                | TARGET_CHAPTER_UID
                                | TARGET_ATTACHMENT_UID
                        );
                        if uid && uint_of(b, &t) != 0 {
                            whole_file = false;
                        }
                    }
                }
                SIMPLE_TAG => {
                    let mut name = String::new();
                    let mut value = None;
                    for s in children(b, c.data, c.end)? {
                        match s.id {
                            TAG_NAME => name = string_of(b, &s),
                            TAG_STRING => value = Some(string_of(b, &s)),
                            _ => {}
                        }
                    }
                    if name == XMP_NAME {
                        xmp = value;
                    }
                }
                _ => {}
            }
        }
        v.push((tag, if whole_file { xmp } else { None }));
    }
    Ok(v)
}

/// The packet, if the file carries our Tag anywhere; the last one wins.
pub fn get(b: &[u8]) -> Result<Option<String>, String> {
    let l = layout(b)?;
    let mut found = None;
    for el in l.children.iter().filter(|e| e.id == TAGS) {
        for (_, xmp) in tags_of(b, el)? {
            if xmp.is_some() {
                found = xmp;
            }
        }
    }
    Ok(found)
}

/// An element of exactly `span` bytes: the element, then a Void; one spare byte is absorbed by a wider size field.
fn fit(id: u32, body: &[u8], span: usize) -> Option<Vec<u8>> {
    let id_len = encode_id(id).len();
    let mut width = size_width(body.len() as u64);
    let mut total = id_len + width + body.len();
    if total > span {
        return None;
    }
    if span - total == 1 {
        if width == 8 {
            return None;
        }
        width += 1;
        total += 1;
    }
    let mut out = element_with(id, body, width);
    if span > total {
        out.extend(void(span - total));
    }
    Some(out)
}

/// The first SeekHead, rebuilt in the space it and the Voids after it occupy: foreign entries kept, the entries
/// that pointed at our old element dropped, one for the new element added when it fits. A CRC-32 or Void inside is
/// dropped, since the rewrite would falsify the one and we pad outside anyway.
fn sync_seekhead(
    out: &mut [u8],
    l: &Layout,
    old_ours: &[u64],
    ours: Option<u64>,
) -> Result<(), String> {
    let Some(k) = l.children.iter().position(|e| e.id == SEEK_HEAD) else {
        return Ok(());
    };
    let sh = l.children[k];
    let span_end = l.children[k + 1..]
        .iter()
        .take_while(|e| e.id == VOID)
        .last()
        .map_or(sh.end, |e| e.end);
    if span_end > out.len() {
        return Ok(());
    }
    let mut foreign: Vec<(u32, u64, Vec<u8>)> = Vec::new();
    let mut dropped = false;
    for e in children(out, sh.data, sh.end)? {
        if e.id != SEEK {
            continue;
        }
        let mut id = 0u32;
        let mut pos = 0u64;
        for c in children(out, e.data, e.end)? {
            match c.id {
                SEEK_ID => id = uint_of(out, &c) as u32,
                SEEK_POSITION => pos = uint_of(out, &c),
                _ => {}
            }
        }
        if id == TAGS && old_ours.contains(&pos) {
            dropped = true;
            continue;
        }
        foreign.push((id, pos, out[e.start..e.end].to_vec()));
    }
    if !dropped && ours.is_none() {
        return Ok(());
    }
    let seek = |id: u32, pos: u64| {
        let body = [
            element(SEEK_ID, &encode_id(id)),
            element(SEEK_POSITION, &uint(pos)),
        ]
        .concat();
        element(SEEK, &body)
    };
    // Foreign entries as they were; failing room, re-encoded as tightly as they go (muxers pad positions to
    // eight bytes), which is what mkvpropedit does too. Nothing fitting and nothing stale to drop leaves the
    // SeekHead exactly as it was, CRC and all.
    let copied: Vec<u8> = foreign.iter().flat_map(|f| f.2.clone()).collect();
    let tight: Vec<u8> = foreign.iter().flat_map(|f| seek(f.0, f.1)).collect();
    let mine = ours.map(|p| seek(TAGS, p)).unwrap_or_default();
    let span = span_end - sh.start;
    let bytes = fit(SEEK_HEAD, &[copied.as_slice(), &mine].concat(), span)
        .or_else(|| fit(SEEK_HEAD, &[tight.as_slice(), &mine].concat(), span))
        .or_else(|| dropped.then(|| fit(SEEK_HEAD, &copied, span)).flatten())
        .or_else(|| dropped.then(|| fit(SEEK_HEAD, &tight, span)).flatten());
    if let Some(bytes) = bytes {
        out[sh.start..span_end].copy_from_slice(&bytes);
    }
    Ok(())
}

/// The file with our Tag holding `xmp` (removed when empty), everything else byte for byte where it was.
pub fn set(b: &[u8], xmp: &str) -> Result<Vec<u8>, String> {
    let l = layout(b)?;
    let keywords = if xmp.is_empty() {
        String::new()
    } else {
        crate::xmp::read(xmp)?.tags.join(", ")
    };
    let mut out = b[..l.seg_end].to_vec();
    let mut old_ours = vec![];
    let mut truncate_to = None;
    for (i, el) in l.children.iter().enumerate() {
        if el.id != TAGS {
            continue;
        }
        let tags = tags_of(b, el)?;
        if tags.iter().all(|t| t.1.is_none()) {
            continue;
        }
        old_ours.push((el.start - l.seg_data) as u64);
        let foreign: Vec<u8> = tags
            .iter()
            .filter(|t| t.1.is_none())
            .flat_map(|t| b[t.0.start..t.0.end].to_vec())
            .collect();
        if !foreign.is_empty() {
            let rebuilt = fit(TAGS, &foreign, el.end - el.start)
                .ok_or("matroska: a Tags element shrank and could not be padded")?;
            out[el.start..el.end].copy_from_slice(&rebuilt);
        } else if i == l.children.len() - 1 {
            truncate_to = Some(el.start);
        } else {
            let pad = void(el.end - el.start);
            out[el.start..el.end].copy_from_slice(&pad);
        }
    }
    if let Some(t) = truncate_to {
        out.truncate(t);
    }
    let ours = if xmp.is_empty() {
        None
    } else {
        let pos = (out.len() - l.seg_data) as u64;
        out.extend(build_tags(xmp, &keywords));
        Some(pos)
    };
    sync_seekhead(&mut out, &l, &old_ours, ours)?;
    if l.seg_known {
        let n = (out.len() - l.seg_data) as u64;
        let width = if n < (1u64 << (7 * l.seg_size_len)) - 1 {
            l.seg_size_len
        } else {
            size_width(n)
        };
        out.splice(l.seg_size_at..l.seg_data, encode_size(n, width));
    }
    out.extend_from_slice(&b[l.seg_end..]);
    Ok(out)
}

/// The bytes a tag write must not change: the header and Segment id, every child except the SeekHead, Voids, CRCs
/// and our own Tag, and anything after the Segment. The same before and after `set`, which is what the writer's
/// verification and the provisional id rely on.
pub fn strip(b: &[u8]) -> Result<Vec<u8>, String> {
    let l = layout(b)?;
    let mut out = b[..l.seg_size_at].to_vec();
    for el in &l.children {
        match el.id {
            SEEK_HEAD | VOID | CRC32 => {}
            TAGS => {
                let tags = tags_of(b, el)?;
                if tags.iter().all(|t| t.1.is_none()) {
                    out.extend_from_slice(&b[el.start..el.end]);
                } else {
                    let foreign: Vec<u8> = tags
                        .iter()
                        .filter(|t| t.1.is_none())
                        .flat_map(|t| b[t.0.start..t.0.end].to_vec())
                        .collect();
                    if !foreign.is_empty() {
                        out.extend(element(TAGS, &foreign));
                    }
                }
            }
            _ => out.extend_from_slice(&b[el.start..el.end]),
        }
    }
    out.extend_from_slice(&b[l.seg_end..]);
    Ok(out)
}

#[cfg(test)]
pub(crate) mod tests_support {
    pub(super) use super::tests::find;
    pub use super::tests::{packet, sample_with};
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: u32 = 0x1549_A966;
    const TRACKS: u32 = 0x1654_AE6B;
    const CLUSTER: u32 = 0x1F43_B675;
    const CUES: u32 = 0x1C53_BB6B;

    fn seek(id: u32, pos: u64) -> Vec<u8> {
        let body = [
            element(SEEK_ID, &encode_id(id)),
            element(SEEK_POSITION, &uint(pos)),
        ]
        .concat();
        element(SEEK, &body)
    }

    fn cluster() -> Vec<u8> {
        let body = [
            element(0xE7, &uint(0)),
            element(0xA3, &[0x81, 0, 0, 0x80, 9, 8, 7, 6, 5, 4, 3, 2, 1]),
        ]
        .concat();
        element(CLUSTER, &body)
    }

    fn foreign_tags() -> Vec<u8> {
        let tag = [
            element(TARGETS, &element(TARGET_TRACK_UID, &uint(1))),
            simple_tag("DURATION", "00:00:01.000000000"),
        ]
        .concat();
        element(TAGS, &element(TAG, &tag))
    }

    /// EBML header, then a Segment: SeekHead (one entry), a Void of `pad` bytes, Info, Tracks, optional foreign
    /// Tags, one Cluster, Cues — the shape of every WebM in the collection.
    fn sample(pad: usize, foreign: bool) -> Vec<u8> {
        sample_with(pad, foreign, false)
    }

    pub fn sample_with(pad: usize, foreign: bool, crc: bool) -> Vec<u8> {
        let header = element(EBML_HEADER, &element(0x4282, b"webm"));
        let mut seekhead = if crc {
            element(CRC32, &[1, 2, 3, 4])
        } else {
            vec![]
        };
        seekhead.extend(seek(INFO, 100));
        let mut body = element(SEEK_HEAD, &seekhead);
        if pad > 0 {
            body.extend(void(pad));
        }
        body.extend(element(INFO, &element(0x2AD7B1, &uint(1_000_000))));
        body.extend(element(TRACKS, b""));
        if foreign {
            body.extend(foreign_tags());
        }
        body.extend(cluster());
        body.extend(element(CUES, b""));
        // an 8-byte Segment size, as every muxer writes it, so the field never has to widen
        [header, element_with(SEGMENT, &body, 8)].concat()
    }

    pub fn packet(tags: &[&str]) -> String {
        let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
        crate::xmp::merge(None, &tags, &Default::default()).unwrap()
    }

    fn seek_entries(b: &[u8]) -> Vec<(u32, u64)> {
        let l = layout(b).unwrap();
        let sh = l.children.iter().find(|e| e.id == SEEK_HEAD).unwrap();
        children(b, sh.data, sh.end)
            .unwrap()
            .iter()
            .map(|e| {
                let mut id = 0;
                let mut pos = 0;
                for c in children(b, e.data, e.end).unwrap() {
                    match c.id {
                        SEEK_ID => id = uint_of(b, &c) as u32,
                        SEEK_POSITION => pos = uint_of(b, &c),
                        _ => {}
                    }
                }
                (id, pos)
            })
            .collect()
    }

    pub(super) fn find(b: &[u8], id: u32) -> El {
        *layout(b)
            .unwrap()
            .children
            .iter()
            .find(|e| e.id == id)
            .unwrap()
    }

    #[test]
    fn vints_round_trip() {
        for n in [0u64, 1, 126, 127, 128, 16382, 16383, 1 << 20, (1 << 56) - 2] {
            let w = size_width(n);
            let enc = encode_size(n, w);
            assert_eq!(read_size(&enc, 0).unwrap(), (Some(n), w), "{n}");
        }
        assert_eq!(read_size(&[0xFF], 0).unwrap(), (None, 1));
        assert_eq!(
            read_size(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF], 0).unwrap(),
            (None, 8)
        );
        for id in [VOID, TAG, INFO, EBML_HEADER] {
            assert_eq!(read_id(&encode_id(id), 0).unwrap().0, id);
        }
        for len in 2..300 {
            let v = void(len);
            assert_eq!(v.len(), len);
            assert_eq!(children(&v, 0, len).unwrap().len(), 1, "{len}");
        }
    }

    #[test]
    fn untagged_reads_nothing_and_an_empty_write_changes_nothing() {
        let s = sample(40, true);
        assert_eq!(get(&s).unwrap(), None);
        assert_eq!(set(&s, "").unwrap(), s);
    }

    #[test]
    fn our_tag_goes_last_and_the_seekhead_points_at_it() {
        let s = sample(40, false);
        let p = packet(&["cat", "loss"]);
        let t = set(&s, &p).unwrap();
        assert_eq!(get(&t).unwrap().as_deref(), Some(p.as_str()));
        let l = layout(&t).unwrap();
        let last = l.children.last().unwrap();
        assert_eq!(last.id, TAGS);
        assert_eq!(l.seg_end, t.len(), "segment size follows");
        let entries = seek_entries(&t);
        assert_eq!(entries[0], (INFO, 100), "foreign entry kept");
        assert_eq!(entries[1], (TAGS, (last.start - l.seg_data) as u64));
        let (c0, c1) = (find(&s, CLUSTER), find(&t, CLUSTER));
        assert_eq!(
            (c0.start, c0.end),
            (c1.start, c1.end),
            "the cluster did not move"
        );
        assert_eq!(s[c0.start..c0.end], t[c1.start..c1.end]);
        assert_eq!(strip(&s).unwrap(), strip(&t).unwrap());
        assert!(
            t.windows(9).any(|w| w == b"cat, loss"),
            "KEYWORDS mirror present"
        );
        let void_before = find(&s, VOID);
        let void_after = find(&t, VOID);
        assert_eq!(
            void_after.end, void_before.end,
            "the padding shrank in place"
        );
    }

    #[test]
    fn a_rewrite_leaves_no_trace_of_the_old_tag() {
        let s = sample(40, true);
        let t1 = set(&s, &packet(&["cat"])).unwrap();
        let t2 = set(&t1, &packet(&["cat", "loss", "reaction"])).unwrap();
        assert_eq!(t2, set(&s, &packet(&["cat", "loss", "reaction"])).unwrap());
        assert_eq!(set(&t2, "").unwrap(), s, "removal restores the original");
        let foreign = find(&s, TAGS);
        assert_eq!(
            s[foreign.start..foreign.end],
            t2[foreign.start..foreign.end]
        );
    }

    #[test]
    fn no_room_beside_the_seekhead_still_tags() {
        let s = sample(0, false);
        let p = packet(&["cat"]);
        let t = set(&s, &p).unwrap();
        assert_eq!(get(&t).unwrap().as_deref(), Some(p.as_str()));
        assert_eq!(seek_entries(&t), vec![(INFO, 100)]);
        assert_eq!(strip(&s).unwrap(), strip(&t).unwrap());
        assert_eq!(set(&t, "").unwrap(), s);
    }

    #[test]
    fn a_narrow_segment_size_field_widens() {
        let s = sample(40, false);
        let l = layout(&s).unwrap();
        let mut narrow = s[..l.seg_size_at].to_vec();
        narrow.extend(encode_size((l.seg_end - l.seg_data) as u64, 1));
        narrow.extend_from_slice(&s[l.seg_data..]);
        let t = set(&narrow, &packet(&["cat"])).unwrap();
        assert_eq!(layout(&t).unwrap().seg_size_len, 2);
        assert!(get(&t).unwrap().is_some());
        assert_eq!(strip(&narrow).unwrap(), strip(&t).unwrap());
    }

    #[test]
    fn one_spare_byte_widens_a_size_field() {
        // padding one byte longer than our Seek entry: the leftover byte is too small for a Void
        let entry = element(
            SEEK,
            &[
                element(SEEK_ID, &encode_id(TAGS)),
                element(SEEK_POSITION, &uint(1)),
            ]
            .concat(),
        );
        let s = sample(entry.len() + 1, false);
        let t = set(&s, &packet(&["cat"])).unwrap();
        let l = layout(&t).unwrap();
        assert_eq!(l.children[0].id, SEEK_HEAD);
        assert_eq!(l.children[1].id, INFO, "no padding left over");
        assert_eq!(seek_entries(&t).len(), 2);
        assert_eq!(set(&t, "").unwrap(), s);
    }

    #[test]
    fn bytes_after_the_segment_stay_after_it() {
        let mut s = sample(40, false);
        s.extend_from_slice(b"junk");
        let t = set(&s, &packet(&["cat"])).unwrap();
        assert!(t.ends_with(b"junk"));
        assert_eq!(layout(&t).unwrap().seg_end, t.len() - 4);
        assert_eq!(get(&t).unwrap().is_some(), true);
        assert_eq!(strip(&s).unwrap(), strip(&t).unwrap());
    }

    #[test]
    fn a_segment_of_unknown_size_is_left_unknown() {
        let s = sample(40, false);
        let l = layout(&s).unwrap();
        let mut u = s[..l.seg_size_at].to_vec();
        u.push(0xFF);
        u.extend_from_slice(&s[l.seg_data..]);
        let t = set(&u, &packet(&["cat"])).unwrap();
        assert_eq!(t[l.seg_size_at], 0xFF);
        assert_eq!(get(&t).unwrap().is_some(), true);
        assert_eq!(strip(&u).unwrap(), strip(&t).unwrap());
    }

    #[test]
    fn a_streamed_cluster_is_refused() {
        let s = sample(40, false);
        let c = find(&s, CLUSTER);
        let mut u = s.clone();
        u[c.start + 4] = 0xFF; // the cluster's 1-byte size becomes "unknown"
        let err = set(&u, &packet(&["cat"])).unwrap_err();
        assert!(err.contains("unknown size"), "{err}");
    }
}
#[cfg(test)]
mod seekhead_tests {
    use super::tests_support::*;
    use super::*;

    #[test]
    fn a_full_seekhead_is_left_exactly_as_it_was() {
        let s = sample_with(0, false, true);
        let t = set(&s, &packet(&["cat"])).unwrap();
        let (a, b) = (find(&s, SEEK_HEAD), find(&t, SEEK_HEAD));
        assert_eq!(s[a.start..a.end], t[b.start..b.end], "CRC and all");
        assert!(get(&t).unwrap().is_some());
        assert_eq!(set(&t, "").unwrap(), s);
    }
}

#[cfg(test)]
mod repack_tests {
    use super::tests_support::*;
    use super::*;

    /// A Seek entry with the position padded to eight bytes, as muxers write them.
    fn seek_wide(id: u32, pos: u64) -> Vec<u8> {
        let body = [
            element(SEEK_ID, &encode_id(id)),
            element(SEEK_POSITION, &pos.to_be_bytes()),
        ]
        .concat();
        element(SEEK, &body)
    }

    #[test]
    fn a_seekhead_without_padding_is_repacked_to_make_room() {
        let s = sample_with(0, false, false);
        let l = layout(&s).unwrap();
        let sh = l.children[0];
        assert_eq!(sh.id, SEEK_HEAD);
        // swap the sample's SeekHead for one with two wide entries and no Void after it
        let wide = element(
            SEEK_HEAD,
            &[seek_wide(0x1549_A966, 100), seek_wide(0x1654_AE6B, 120)].concat(),
        );
        let mut w = s[..sh.start].to_vec();
        w.extend_from_slice(&wide);
        w.extend_from_slice(&s[sh.end..]);
        let n = (w.len() - l.seg_data) as u64;
        w.splice(l.seg_size_at..l.seg_data, encode_size(n, 8));
        let t = set(&w, &packet(&["cat"])).unwrap();
        let lt = layout(&t).unwrap();
        assert_eq!(
            lt.children[0].end,
            lt.children[0].start + wide.len(),
            "same span"
        );
        let entries = children(&t, lt.children[0].data, lt.children[0].end).unwrap();
        assert_eq!(entries.len(), 3, "two foreign entries repacked, ours added");
        assert!(get(&t).unwrap().is_some());
        assert_eq!(strip(&w).unwrap(), strip(&t).unwrap());
        let _ = find(&t, TAGS);
    }
}

#[cfg(test)]
mod generated_tests {
    //! Segments of random shape, the invariants every write must keep. Bounded and seeded, like property_tests.
    use super::*;

    const INFO: u32 = 0x1549_A966;
    const TRACKS: u32 = 0x1654_AE6B;
    const CLUSTER: u32 = 0x1F43_B675;
    const CUES: u32 = 0x1C53_BB6B;
    const CHAPTERS: u32 = 0x1043_A770;
    const ATTACHMENTS: u32 = 0x1941_A469;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                self.next() as usize % n
            }
        }
        fn chance(&mut self, one_in: usize) -> bool {
            self.below(one_in) == 0
        }
    }

    fn seek(id: u32, pos: u64, wide: bool) -> Vec<u8> {
        let pos = if wide {
            pos.to_be_bytes().to_vec()
        } else {
            uint(pos)
        };
        element(
            SEEK,
            &[
                element(SEEK_ID, &encode_id(id)),
                element(SEEK_POSITION, &pos),
            ]
            .concat(),
        )
    }
    fn cluster(rng: &mut Rng) -> Vec<u8> {
        let mut body = element(0xE7, &uint(rng.below(90_000) as u64));
        for _ in 0..1 + rng.below(3) {
            let mut block = vec![0x81, 0, 0, 0x80];
            block.extend((0..rng.below(300)).map(|_| rng.next() as u8));
            body.extend(element(0xA3, &block));
        }
        element(CLUSTER, &body)
    }
    /// Tags another program wrote: aimed at a track, or at the whole file but without an XMP SimpleTag.
    fn foreign_tags(rng: &mut Rng) -> Vec<u8> {
        let mut tags = vec![];
        if rng.chance(2) {
            let tag = [
                element(TARGETS, &element(TARGET_TRACK_UID, &uint(1))),
                simple_tag("DURATION", "00:00:01.000000000"),
            ]
            .concat();
            tags.extend(element(TAG, &tag));
        }
        if rng.chance(2) {
            let tag = [
                element(TARGETS, &element(TARGET_TYPE_VALUE, &[WHOLE_FILE])),
                simple_tag("TITLE", "Foreign title 猫"),
            ]
            .concat();
            tags.extend(element(TAG, &tag));
        }
        if tags.is_empty() {
            tags = element(TAG, &simple_tag("ENCODER", "test"));
        }
        element(TAGS, &tags)
    }
    /// A file with no tag of ours, of the shapes muxers produce and a few they do not; returns the bytes and how
    /// many of them follow the Segment.
    fn random_file(rng: &mut Rng) -> (Vec<u8>, usize) {
        let doctype: &[u8] = if rng.chance(2) { b"webm" } else { b"matroska" };
        let header = element(EBML_HEADER, &element(0x4282, doctype));
        let mut body = vec![];
        if !rng.chance(5) {
            let mut sh = vec![];
            if rng.chance(4) {
                sh.extend(element(CRC32, &[1, 2, 3, 4]));
            }
            for _ in 0..rng.below(4) {
                let id = [INFO, TRACKS, CUES, CHAPTERS][rng.below(4)];
                sh.extend(seek(id, rng.below(5000) as u64, rng.chance(2)));
            }
            body.extend(element(SEEK_HEAD, &sh));
        }
        if rng.chance(2) {
            body.extend(void(2 + rng.below(200)));
        }
        body.extend(element(INFO, &element(0x2AD7B1, &uint(1_000_000))));
        body.extend(element(TRACKS, b""));
        if rng.chance(3) {
            body.extend(foreign_tags(rng));
        }
        for _ in 0..1 + rng.below(5) {
            body.extend(cluster(rng));
        }
        if !rng.chance(4) {
            body.extend(element(CUES, b""));
        }
        if rng.chance(4) {
            body.extend(element(CHAPTERS, b""));
        }
        if rng.chance(5) {
            body.extend(element(ATTACHMENTS, &[7; 40]));
        }
        if rng.chance(4) {
            body.extend(void(2 + rng.below(20)));
        }
        let unknown = rng.chance(6);
        let seg = if unknown {
            let mut s = encode_id(SEGMENT);
            s.push(0xFF); // unknown size, as a stream capture leaves it: it reaches the end of the file
            s.extend(&body);
            s
        } else {
            let width = (1 + rng.below(8)).max(size_width(body.len() as u64));
            element_with(SEGMENT, &body, width)
        };
        let mut out = [header, seg].concat();
        let trailing = if !unknown && rng.chance(4) { 14 } else { 0 };
        out.extend_from_slice(&b"trailing bytes"[..trailing]);
        (out, trailing)
    }
    fn packet(rng: &mut Rng) -> String {
        let pool = [
            "cat",
            "loss",
            "猫は箱の中",
            "odd \"quoted\" & <tag>",
            "reaction:laugh",
        ];
        let tags: Vec<String> = (0..1 + rng.below(3))
            .map(|_| pool[rng.below(pool.len())].to_string())
            .collect();
        crate::xmp::merge(None, &tags, &Default::default()).unwrap()
    }
    fn clusters(b: &[u8]) -> Vec<(usize, usize)> {
        layout(b)
            .unwrap()
            .children
            .iter()
            .filter(|e| e.id == CLUSTER)
            .map(|e| (e.start, e.end))
            .collect()
    }
    fn seek_entries(b: &[u8]) -> Option<Vec<(u32, u64)>> {
        let l = layout(b).unwrap();
        let sh = l.children.iter().find(|e| e.id == SEEK_HEAD)?;
        Some(
            children(b, sh.data, sh.end)
                .unwrap()
                .iter()
                .filter(|e| e.id == SEEK)
                .map(|e| {
                    let (mut id, mut pos) = (0, 0);
                    for c in children(b, e.data, e.end).unwrap() {
                        match c.id {
                            SEEK_ID => id = uint_of(b, &c) as u32,
                            SEEK_POSITION => pos = uint_of(b, &c),
                            _ => {}
                        }
                    }
                    (id, pos)
                })
                .collect(),
        )
    }

    #[test]
    fn generated_segments_keep_every_invariant_across_tag_writes() {
        let mut rng = Rng(0xb123_4567_89ab_cdef);
        for case in 0..600 {
            let (s, trailing) = random_file(&mut rng);
            assert_eq!(get(&s).unwrap(), None, "case {case}");
            assert_eq!(
                set(&s, "").unwrap(),
                s,
                "case {case}: an empty write is a no-op"
            );
            let p = packet(&mut rng);
            let t = set(&s, &p).unwrap();
            assert_eq!(get(&t).unwrap().as_deref(), Some(p.as_str()), "case {case}");
            assert_eq!(
                strip(&s).unwrap(),
                strip(&t).unwrap(),
                "case {case}: media moved"
            );
            assert_eq!(set(&t, &p).unwrap(), t, "case {case}: not idempotent");
            assert_eq!(clusters(&s), clusters(&t), "case {case}: a cluster moved");
            let lt = layout(&t).unwrap();
            assert_eq!(lt.seg_end + trailing, t.len(), "case {case}: segment size");
            assert_eq!(
                &t[lt.seg_end..],
                &s[s.len() - trailing..],
                "case {case}: tail"
            );
            assert_eq!(
                lt.children.last().unwrap().id,
                TAGS,
                "case {case}: ours is last"
            );
            let ours = (lt.children.last().unwrap().start - lt.seg_data) as u64;
            // the SeekHead either points at us or was left byte for byte
            let (ls, sh_s) = (layout(&s).unwrap(), seek_entries(&s));
            if let (Some(before), Some(after)) = (sh_s, seek_entries(&t)) {
                let a = ls.children.iter().find(|e| e.id == SEEK_HEAD).unwrap();
                let b = lt.children.iter().find(|e| e.id == SEEK_HEAD).unwrap();
                assert_eq!((a.start, b.start), (b.start, a.start), "case {case}");
                if s[a.start..a.end] != t[b.start..b.end] {
                    assert_eq!(after.last(), Some(&(TAGS, ours)), "case {case}: {after:?}");
                    assert_eq!(
                        &after[..after.len() - 1],
                        &before[..],
                        "case {case}: foreign entries"
                    );
                }
            }
            // a second write replaces, never stacks; removal leaves the media and the length as they were
            let p2 = packet(&mut rng);
            let t2 = set(&t, &p2).unwrap();
            assert_eq!(
                get(&t2).unwrap().as_deref(),
                Some(p2.as_str()),
                "case {case}"
            );
            assert_eq!(t2, set(&s, &p2).unwrap(), "case {case}: history shows");
            let back = set(&t2, "").unwrap();
            assert_eq!(get(&back).unwrap(), None, "case {case}");
            assert_eq!(back.len(), s.len(), "case {case}: removal length");
            assert_eq!(strip(&back).unwrap(), strip(&s).unwrap(), "case {case}");
            assert_eq!(clusters(&back), clusters(&s), "case {case}");
            // another program appended an element after ours: ours is voided in place and a fresh one goes last
            if case % 3 == 0 {
                let mut plus = t[..lt.seg_end].to_vec();
                plus.extend(element(CHAPTERS, &[1; 12]));
                if lt.seg_known {
                    let n = (plus.len() - lt.seg_data) as u64;
                    plus.splice(
                        lt.seg_size_at..lt.seg_data,
                        encode_size(n, 8.max(lt.seg_size_len)),
                    );
                    // the segment size field may have grown: re-read the layout rather than trust offsets
                }
                plus.extend_from_slice(&t[lt.seg_end..]);
                let lp = layout(&plus).unwrap();
                let t3 = set(&plus, &p2).unwrap();
                assert_eq!(
                    get(&t3).unwrap().as_deref(),
                    Some(p2.as_str()),
                    "case {case}"
                );
                let l3 = layout(&t3).unwrap();
                let tags: Vec<_> = l3.children.iter().filter(|e| e.id == TAGS).collect();
                let ours_in: Vec<_> = tags
                    .iter()
                    .filter(|e| tags_of(&t3, e).unwrap().iter().any(|t| t.1.is_some()))
                    .collect();
                assert_eq!(ours_in.len(), 1, "case {case}: one tag of ours");
                assert_eq!(l3.children.last().unwrap().id, TAGS, "case {case}");
                assert_eq!(strip(&plus).unwrap(), strip(&t3).unwrap(), "case {case}");
                assert_eq!(clusters(&plus), clusters(&t3), "case {case}");
                let _ = lp;
            }
        }
    }
}
