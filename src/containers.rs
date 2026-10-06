//! Per-format XMP placement: read the packet out of a file's bytes, or produce new bytes with a
//! packet spliced in. Pixels/samples are never touched; only metadata segments/chunks/boxes move.
use img_parts::jpeg::{markers, Jpeg, JpegSegment};
use img_parts::png::{Png, PngChunk};
use img_parts::riff::{RiffChunk, RiffContent};
use img_parts::webp::WebP;
use img_parts::Bytes;

pub const XMP_NS_HDR: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
const PNG_KW: &[u8] = b"XML:com.adobe.xmp\0";
const GIF_APP: &[u8] = b"XMP DataXMP";
const TRAILER_MAGIC: &[u8] = b"MEMETAG1";
/// XMP Part 3: the ISO BMFF uuid box that carries XMP (BE7ACFCB-97A9-42E8-9C71-999491E3AFAC).
const XMP_UUID: [u8; 16] = [
    0xBE, 0x7A, 0xCF, 0xCB, 0x97, 0xA9, 0x42, 0xE8, 0x9C, 0x71, 0x99, 0x94, 0x91, 0xE3, 0xAF, 0xAC,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Jpeg,
    Png,
    WebP,
    Gif,
    Mp4,
    /// A still AVIF (major brand `avif`): an AV1 picture in an ISO BMFF box tree, so XMP lives in the same uuid box
    /// as MP4, but it is an image for scanning, thumbnails, hashing and OCR. Decoded by ffmpeg (`decode`), since the
    /// image crate carries no AV1 decoder here. An animated one (major brand `avis`) stays a video.
    Avif,
    Matroska,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Jpeg => "jpeg",
            Kind::Png => "png",
            Kind::WebP => "webp",
            Kind::Gif => "gif",
            Kind::Mp4 => "mp4",
            Kind::Avif => "avif",
            Kind::Matroska => "mkv",
            Kind::Other => "other",
        }
    }
    pub fn is_image(self) -> bool {
        matches!(
            self,
            Kind::Jpeg | Kind::Png | Kind::WebP | Kind::Gif | Kind::Avif
        )
    }
    pub fn is_video(self) -> bool {
        matches!(self, Kind::Mp4 | Kind::Matroska)
    }
}

pub fn sniff(b: &[u8]) -> Kind {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Kind::Jpeg
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Kind::Png
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Kind::WebP
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Kind::Gif
    } else if b.len() > 12 && &b[4..8] == b"ftyp" {
        if &b[8..12] == b"avif" {
            Kind::Avif
        } else {
            Kind::Mp4
        }
    } else if b.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        Kind::Matroska
    } else {
        Kind::Other
    }
}

/// True for a PNG that carries an animation control chunk (APNG). Walks the chunk list up to the first IDAT;
/// the spec puts acTL before it, so a still PNG costs one IHDR hop. Same container otherwise: XMP goes in the same iTXt.
pub fn is_apng(b: &[u8]) -> bool {
    if !b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return false;
    }
    let mut i = 8;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        match &b[i + 4..i + 8] {
            b"acTL" => return true,
            b"IDAT" | b"IEND" => return false,
            _ => {}
        }
        i = match i.checked_add(12 + len) {
            Some(n) => n,
            None => return false,
        };
    }
    false
}

/// The picture in a file's bytes, whatever the container: the image crate for what it decodes, ffmpeg for AVIF.
/// Every place that turns file bytes into pixels (thumbnails, hashes, the pixel id, OCR, the clipboard) goes
/// through here, so a new format is one arm.
pub fn decode(b: &[u8]) -> Result<image::DynamicImage, String> {
    match sniff(b) {
        Kind::Avif => ffmpeg_decode(b),
        _ => image::load_from_memory(trailer_strip(b)).map_err(|e| e.to_string()),
    }
}

/// One PNG frame out of ffmpeg, from a temporary copy of the bytes on disk. Not stdin: the mov demuxer needs
/// to seek for AVIFs with more than one item (alpha plane, grid tiles), and from a pipe it fails with "partial file"
/// (decode failures must not leave supported images permanently pending).
fn ffmpeg_decode(b: &[u8]) -> Result<image::DynamicImage, String> {
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "memetag-{}-{}.avif",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&tmp, b).map_err(|e| format!("ffmpeg tmp: {e}"))?;
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&tmp)
        .args([
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-c:v",
            "png",
            "pipe:1",
        ])
        .output();
    let _ = std::fs::remove_file(&tmp);
    let out = out.map_err(|e| format!("ffmpeg: {e}"))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    image::load_from_memory(&out.stdout).map_err(|e| format!("ffmpeg png: {e}"))
}

pub fn get_xmp(b: &[u8]) -> Result<Option<String>, String> {
    let s = |v: &[u8]| String::from_utf8_lossy(v).into_owned();
    Ok(match sniff(b) {
        Kind::Jpeg => Jpeg::from_bytes(Bytes::from(b.to_vec()))
            .map_err(|e| format!("jpeg: {e}"))?
            .segments()
            .iter()
            .find(|sg| sg.marker() == markers::APP1 && sg.contents().starts_with(XMP_NS_HDR))
            .map(|sg| s(&sg.contents()[XMP_NS_HDR.len()..])),
        Kind::Png => Png::from_bytes(Bytes::from(b.to_vec()))
            .map_err(|e| format!("png: {e}"))?
            .chunks()
            .iter()
            .find(|c| c.kind() == *b"iTXt" && c.contents().starts_with(PNG_KW))
            .and_then(|c| png_itxt_text(c.contents())),
        Kind::WebP => WebP::from_bytes(Bytes::from(b.to_vec()))
            .map_err(|e| format!("webp: {e}"))?
            .chunks()
            .iter()
            .find(|c| c.id() == *b"XMP ")
            .and_then(|c| c.content().data())
            .map(|d| s(d)),
        Kind::Gif => {
            let (_, spans) = gif_scan(b)?;
            spans.iter().find_map(|&(st, en)| gif_packet(b, st, en))
        }
        Kind::Mp4 | Kind::Avif => mp4_boxes(b)?
            .into_iter()
            .find(|(st, en, ty)| *ty == *b"uuid" && en - st >= 24 && b[st + 8..st + 24] == XMP_UUID)
            .map(|(st, en, _)| s(&b[st + 24..en])),
        Kind::Matroska => crate::matroska::get(b)?,
        Kind::Other => trailer_get(b).map(s),
    })
}

pub fn set_xmp(b: &[u8], xmp: &str) -> Result<Vec<u8>, String> {
    let bytes = Bytes::from(b.to_vec());
    Ok(match sniff(b) {
        Kind::Jpeg => jpeg_set(bytes, xmp)?,
        Kind::Png => png_set(bytes, xmp)?,
        Kind::WebP => webp_set(bytes, xmp)?,
        Kind::Gif => gif_set(b, xmp)?,
        Kind::Mp4 | Kind::Avif => mp4_set(b, xmp)?,
        Kind::Matroska => crate::matroska::set(b, xmp)?,
        Kind::Other => trailer_set(b, xmp),
    })
}

/// The bytes that must not change when tags are written: everything except the XMP carrier.
/// Used to verify non-decodable files (videos, odd BMPs) after a write.
pub fn strip_xmp(b: &[u8]) -> Result<Vec<u8>, String> {
    Ok(match sniff(b) {
        Kind::Other => trailer_strip(b).to_vec(),
        Kind::Mp4 | Kind::Avif => {
            let mut out = Vec::with_capacity(b.len());
            for (st, en, ty) in mp4_boxes(b)? {
                if !(ty == *b"uuid" && en - st >= 24 && b[st + 8..st + 24] == XMP_UUID) {
                    out.extend_from_slice(&b[st..en]);
                }
            }
            out
        }
        Kind::Matroska => crate::matroska::strip(b)?,
        _ => set_xmp(b, "")?, // same carrier with an empty packet; good enough for a structural compare
    })
}

// ---------- JPEG ----------
fn jpeg_set(b: Bytes, xmp: &str) -> Result<Vec<u8>, String> {
    let mut j = Jpeg::from_bytes(b).map_err(|e| format!("jpeg: {e}"))?;
    let segs = j.segments_mut();
    segs.retain(|s| !(s.marker() == markers::APP1 && s.contents().starts_with(XMP_NS_HDR)));
    let mut pos = 0;
    while pos < segs.len()
        && (segs[pos].marker() == markers::APP0
            || (segs[pos].marker() == markers::APP1
                && segs[pos].contents().starts_with(b"Exif\0\0")))
    {
        pos += 1;
    }
    if !xmp.is_empty() {
        let mut c = Vec::from(XMP_NS_HDR);
        c.extend_from_slice(xmp.as_bytes());
        if c.len() > 65533 {
            return Err(
                "xmp packet too large for one APP1 segment (extended XMP not implemented)".into(),
            );
        }
        segs.insert(
            pos,
            JpegSegment::new_with_contents(markers::APP1, Bytes::from(c)),
        );
    }
    let mut out = Vec::new();
    j.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("jpeg encode: {e}"))?;
    Ok(out)
}

// ---------- PNG ----------
fn png_itxt_text(c: &[u8]) -> Option<String> {
    // keyword\0 compression-flag compression-method language\0 translated-keyword\0 text
    let mut i = c.iter().position(|&x| x == 0)? + 1;
    if c.get(i) != Some(&0) {
        return None;
    }
    i += 2;
    i += c[i..].iter().position(|&x| x == 0)? + 1;
    i += c[i..].iter().position(|&x| x == 0)? + 1;
    Some(String::from_utf8_lossy(&c[i..]).into_owned())
}
/// Offset just past the IEND chunk, where the PNG parser stops. Anything after it (23 files in the collection
/// on 2026-09-22: appended archives, junk from downloaders) is not media, but it is the file's bytes and it stays.
fn png_end(b: &[u8]) -> Option<usize> {
    let mut i = 8;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes(b[i..i + 4].try_into().ok()?) as usize;
        let end = i.checked_add(12)?.checked_add(len)?;
        if end > b.len() {
            return None;
        }
        if &b[i + 4..i + 8] == b"IEND" {
            return Some(end);
        }
        i = end;
    }
    None
}
fn png_set(b: Bytes, xmp: &str) -> Result<Vec<u8>, String> {
    let tail: Vec<u8> = png_end(&b).map(|e| b[e..].to_vec()).unwrap_or_default();
    let mut p = Png::from_bytes(b).map_err(|e| format!("png: {e}"))?;
    let ch = p.chunks_mut();
    ch.retain(|c| !(c.kind() == *b"iTXt" && c.contents().starts_with(PNG_KW)));
    if !xmp.is_empty() {
        let mut c = Vec::from(PNG_KW);
        c.extend_from_slice(&[0, 0, 0, 0]);
        c.extend_from_slice(xmp.as_bytes());
        let pos = if !ch.is_empty() && ch[0].kind() == *b"IHDR" {
            1
        } else {
            0
        };
        ch.insert(pos, PngChunk::new(*b"iTXt", Bytes::from(c)));
    }
    let mut out = Vec::new();
    p.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("png encode: {e}"))?;
    out.extend_from_slice(&tail);
    Ok(out)
}

// ---------- WebP ----------
fn webp_set(b: Bytes, xmp: &str) -> Result<Vec<u8>, String> {
    // The RIFF header says where the container ends; bytes after that are kept, as for PNG.
    let tail: Vec<u8> = b
        .get(4..8)
        .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as usize)
        .map(|n| 8 + n + (n & 1))
        .filter(|e| *e < b.len())
        .map(|e| b[e..].to_vec())
        .unwrap_or_default();
    let mut w = WebP::from_bytes(b).map_err(|e| format!("webp: {e}"))?;
    w.remove_chunks_by_id(*b"XMP ");
    // img-parts 0.3.3 reads the VP8X canvas at the wrong offset, so an existing header is kept verbatim.
    let existing: Option<Vec<u8>> = w
        .chunk_by_id(*b"VP8X")
        .and_then(|c| c.content().data())
        .filter(|d| d.len() >= 10)
        .map(|d| d[..10].to_vec());
    let mut content = match existing {
        Some(d) => d,
        None => {
            let (wd, ht) = simple_webp_dimensions(&w)
                .ok_or("webp: cannot read dimensions from the VP8/VP8L bitstream")?;
            let mut flags = [0u8; 4];
            if w.has_chunk(*b"ICCP") {
                flags[0] |= 0x20;
            }
            if w.has_chunk(*b"EXIF") {
                flags[0] |= 0x08;
            }
            if w.has_chunk(*b"ALPH") {
                flags[0] |= 0x10;
            }
            if w.has_chunk(*b"ANIM") {
                flags[0] |= 0x02;
            }
            if let Some(d) = w.chunk_by_id(*b"VP8L").and_then(|c| c.content().data()) {
                if d.len() > 4 && d[4] & 0x10 != 0 {
                    flags[0] |= 0x10;
                }
            }
            let mut c = Vec::with_capacity(10);
            c.extend_from_slice(&flags);
            c.extend_from_slice(&(wd - 1).to_le_bytes()[..3]);
            c.extend_from_slice(&(ht - 1).to_le_bytes()[..3]);
            c
        }
    };
    if xmp.is_empty() {
        content[0] &= !0x04;
    } else {
        content[0] |= 0x04;
    }
    w.remove_chunks_by_id(*b"VP8X");
    w.chunks_mut().insert(
        0,
        RiffChunk::new(*b"VP8X", RiffContent::Data(Bytes::from(content))),
    );
    if !xmp.is_empty() {
        w.chunks_mut().push(RiffChunk::new(
            *b"XMP ",
            RiffContent::Data(Bytes::from(xmp.as_bytes().to_vec())),
        ));
    }
    let mut out = Vec::new();
    w.encoder()
        .write_to(&mut out)
        .map_err(|e| format!("webp encode: {e}"))?;
    out.extend_from_slice(&tail);
    Ok(out)
}

/// Dimensions of a simple-format WebP from its bitstream header, bounds-checked (img-parts' own reader panics on short chunks).
fn simple_webp_dimensions(w: &WebP) -> Option<(u32, u32)> {
    if let Some(d) = w.chunk_by_id(*b"VP8 ").and_then(|c| c.content().data()) {
        // 3-byte frame tag, start code 9d 01 2a, then 14-bit width and height (low 14 bits of two LE u16)
        if d.len() < 10 || d[3..6] != [0x9D, 0x01, 0x2A] {
            return None;
        }
        let wd = (u16::from_le_bytes([d[6], d[7]]) & 0x3FFF) as u32;
        let ht = (u16::from_le_bytes([d[8], d[9]]) & 0x3FFF) as u32;
        return if wd > 0 && ht > 0 {
            Some((wd, ht))
        } else {
            None
        };
    }
    if let Some(d) = w.chunk_by_id(*b"VP8L").and_then(|c| c.content().data()) {
        if d.len() < 5 || d[0] != 0x2F {
            return None;
        }
        let bits = u32::from_le_bytes([d[1], d[2], d[3], d[4]]);
        return Some(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1));
    }
    None
}

// ---------- GIF ----------
fn gif_skip_subblocks(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let n = *b.get(i)? as usize;
        i += 1;
        if n == 0 {
            return Some(i);
        }
        i += n;
    }
}
/// (offset where blocks start, the span of every XMP application extension, in file order). The walk over sub-block
/// lengths is the one every decoder makes to skip a block, so a span is exactly what a decoder skips, whatever the
/// block holds. The scan stops at a byte that is no block introducer and leaves the rest of the file as it is: seven
/// GIFs in the collection end in `<` where the `;` trailer belongs (2026-09-22), and every viewer shows them.
fn gif_scan(b: &[u8]) -> Result<(usize, Vec<(usize, usize)>), String> {
    if b.len() < 13 {
        return Err("gif: too short".into());
    }
    let flags = b[10];
    let mut i = 13;
    if flags & 0x80 != 0 {
        i += 3 * (1usize << ((flags & 7) as usize + 1));
    }
    if i > b.len() {
        return Err("gif: header claims a color table past the end of the file".into());
    }
    let first = i;
    let mut found = vec![];
    while i < b.len() {
        match b[i] {
            0x3B => break,
            0x2C => {
                if i + 10 > b.len() {
                    return Err("gif: truncated".into());
                }
                let f = b[i + 9];
                i += 10;
                if f & 0x80 != 0 {
                    i += 3 * (1usize << ((f & 7) as usize + 1));
                }
                i += 1;
                if i > b.len() {
                    return Err("gif: truncated image descriptor".into());
                }
                i = gif_skip_subblocks(b, i).ok_or("gif: truncated image")?;
            }
            0x21 => {
                let st = i;
                let label = *b.get(i + 1).ok_or("gif: truncated")?;
                i += 2;
                let is_xmp = label == 0xFF
                    && b.get(i) == Some(&0x0B)
                    && b.get(i + 1..i + 12) == Some(GIF_APP);
                i = gif_skip_subblocks(b, i).ok_or("gif: truncated extension")?;
                if is_xmp {
                    found.push((st, i));
                }
            }
            _ => break,
        }
    }
    Ok((first, found))
}
/// The packet in one XMP application extension, when there is one to merge into: the raw bytes minus the 258-byte
/// magic trailer (the form Adobe and memetag write, where the packet's own bytes stand in for sub-block lengths),
/// else the sub-block contents joined (the form generic writers produce: three files in the collection, 2026-09-22,
/// and Fireworks wrote one per frame). Fragments left by re-encoders (nine files) hold no RDF and are not packets.
fn gif_packet(b: &[u8], st: usize, en: usize) -> Option<String> {
    let raw = &b[st + 14..en];
    let trailer: Vec<u8> = std::iter::once(1)
        .chain((0..=255u8).rev())
        .chain(std::iter::once(0))
        .collect();
    // Fireworks CS3 wrote the trailer with 0x00 where its 0x3B belongs (the byte that is also the GIF trailer,
    // zeroed by something on the way); one file in the collection carries 89 such copies (2026-09-22)
    let has_trailer = raw.len() >= trailer.len()
        && raw[raw.len() - trailer.len()..]
            .iter()
            .zip(&trailer)
            .enumerate()
            .all(|(k, (a, want))| a == want || (k == 197 && *a == 0));
    let bytes: Vec<u8> = match has_trailer {
        true => raw[..raw.len() - trailer.len()].to_vec(),
        false => {
            let mut v = vec![];
            let mut i = st + 14;
            while i < en {
                let n = b[i] as usize;
                i += 1;
                if n == 0 {
                    break;
                }
                v.extend_from_slice(&b[i..(i + n).min(en)]);
                i += n;
            }
            v
        }
    };
    let s = String::from_utf8_lossy(&bytes).into_owned();
    // usable means mergeable: what `set` will have to do with it
    (s.contains("RDF") && crate::xmp::merge(Some(&s), &[], &Default::default()).is_ok())
        .then_some(s)
}
fn gif_set(b: &[u8], xmp: &str) -> Result<Vec<u8>, String> {
    let (first, spans) = gif_scan(b)?;
    let mut body = b.to_vec();
    for &(s, e) in spans.iter().rev() {
        body.drain(s..e);
    }
    let mut out = Vec::with_capacity(body.len() + xmp.len() + 300);
    out.extend_from_slice(&body[..first]);
    if !xmp.is_empty() {
        out.extend_from_slice(&[0x21, 0xFF, 0x0B]);
        out.extend_from_slice(GIF_APP);
        out.extend_from_slice(xmp.as_bytes());
        out.push(0x01);
        for v in (0..=0xFFu8).rev() {
            out.push(v);
        }
        out.push(0x00); // the "magic trailer"
    }
    out.extend_from_slice(&body[first..]);
    Ok(out)
}

// ---------- ISO BMFF (MP4/MOV): top-level uuid box ----------
fn mp4_boxes(b: &[u8]) -> Result<Vec<(usize, usize, [u8; 4])>, String> {
    let mut v = vec![];
    let mut i = 0;
    while i + 8 <= b.len() {
        let sz32 = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        let ty: [u8; 4] = b[i + 4..i + 8].try_into().unwrap();
        let sz = match sz32 {
            0 => b.len() - i,
            1 => {
                if i + 16 > b.len() {
                    return Err("mp4: truncated largesize".into());
                }
                u64::from_be_bytes(b[i + 8..i + 16].try_into().unwrap()) as usize
            }
            n => n,
        };
        let header = if sz32 == 1 { 16 } else { 8 };
        if sz < header || sz > b.len() - i {
            return Err(format!("mp4: bad box size {sz} at {i}"));
        }
        v.push((i, i + sz, ty));
        i += sz;
    }
    if i != b.len() {
        return Err("mp4: trailing bytes".into());
    }
    Ok(v)
}
fn mp4_set(b: &[u8], xmp: &str) -> Result<Vec<u8>, String> {
    let boxes = mp4_boxes(b)?;
    // Chunk offsets (stco/co64, and iloc in AVIF) are absolute. Dropping an XMP box that sits ahead of the
    // media would shift every later byte and no verification here can see it (videos do not decode), so a
    // foreign box in that position is refused. Ours always goes at the end. One such file on 2026-09-22.
    let is_xmp = |st: usize, en: usize, ty: &[u8; 4]| {
        *ty == *b"uuid" && en - st >= 24 && b[st + 8..st + 24] == XMP_UUID
    };
    if let Some(first) = boxes.iter().position(|(st, en, ty)| is_xmp(*st, *en, ty)) {
        if boxes[first..]
            .iter()
            .any(|(_, _, ty)| matches!(ty, b"mdat" | b"moov" | b"meta"))
        {
            return Err(
                "mp4: an XMP box written by another program sits ahead of the media; moving it would shift the \
                 chunk offsets, so this file is not tagged in place (remux it first, e.g. ffmpeg -c copy)"
                    .into(),
            );
        }
    }
    let mut out = Vec::with_capacity(b.len() + xmp.len() + 32);
    for (st, en, ty) in &boxes {
        if !is_xmp(*st, *en, ty) {
            out.extend_from_slice(&b[*st..*en]);
        }
    }
    if !xmp.is_empty() {
        let sz = 8 + 16 + xmp.len();
        if sz > u32::MAX as usize {
            return Err("mp4: xmp too large".into());
        }
        out.extend_from_slice(&(sz as u32).to_be_bytes());
        out.extend_from_slice(b"uuid");
        out.extend_from_slice(&XMP_UUID);
        out.extend_from_slice(xmp.as_bytes());
    }
    Ok(out)
}

// ---------- fallback: trailer after EOF ----------
pub fn trailer_strip(b: &[u8]) -> &[u8] {
    if b.len() >= 20 && &b[b.len() - 8..] == TRAILER_MAGIC {
        let n = u32::from_le_bytes(b[b.len() - 12..b.len() - 8].try_into().unwrap()) as usize;
        let start = b.len().checked_sub(12 + n + 8);
        if let Some(st) = start {
            if &b[st..st + 8] == TRAILER_MAGIC {
                return &b[..st];
            }
        }
    }
    b
}
fn trailer_set(b: &[u8], xmp: &str) -> Vec<u8> {
    let mut out = trailer_strip(b).to_vec();
    if xmp.is_empty() {
        return out;
    }
    out.extend_from_slice(TRAILER_MAGIC);
    out.extend_from_slice(xmp.as_bytes());
    out.extend_from_slice(&(xmp.len() as u32).to_le_bytes());
    out.extend_from_slice(TRAILER_MAGIC);
    out
}
fn trailer_get(b: &[u8]) -> Option<&[u8]> {
    let base = trailer_strip(b);
    if base.len() == b.len() {
        None
    } else {
        Some(&b[base.len() + 8..b.len() - 12])
    }
}

#[cfg(test)]
mod apng_tests {
    use super::*;
    fn chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = (body.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v.extend_from_slice(&[0; 4]);
        v
    }
    fn png(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        for c in chunks {
            v.extend_from_slice(c);
        }
        v
    }
    #[test]
    fn apng_is_a_png_with_an_actl_chunk_before_idat() {
        let ihdr = chunk(b"IHDR", &[0; 13]);
        assert!(is_apng(&png(&[
            ihdr.clone(),
            chunk(b"acTL", &[0; 8]),
            chunk(b"IDAT", b"x")
        ])));
        assert!(
            !is_apng(&png(&[
                ihdr.clone(),
                chunk(b"IDAT", b"x"),
                chunk(b"IEND", b"")
            ])),
            "a still PNG"
        );
        assert!(
            !is_apng(&png(&[
                ihdr.clone(),
                chunk(b"IDAT", b"x"),
                chunk(b"acTL", &[0; 8])
            ])),
            "acTL after IDAT is not honoured, and the walk stops at IDAT"
        );
        assert!(
            !is_apng(&png(&[ihdr.clone()])[..20]),
            "truncated inside a chunk header"
        );
        assert!(
            !is_apng(&png(&[chunk(b"IHDR", &[0; 13])[..12].to_vec()])),
            "chunk longer than the buffer"
        );
        assert!(!is_apng(b"GIF89a"), "not a PNG");
        assert_eq!(
            sniff(&png(&[ihdr, chunk(b"acTL", &[0; 8])])),
            Kind::Png,
            "still the PNG container for XMP purposes"
        );
    }
}

#[cfg(test)]
mod tail_and_offset_tests {
    use super::*;
    const PACKET: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"/></rdf:RDF></x:xmpmeta>";
    fn png_bytes() -> Vec<u8> {
        let mut c = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(4, 4, image::Rgb([9, 8, 7]))
            .write_to(&mut c, image::ImageFormat::Png)
            .unwrap();
        c.into_inner()
    }
    fn mp4_box(ty: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((8 + body.len()) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(ty);
        v.extend_from_slice(body);
        v
    }
    fn xmp_box() -> Vec<u8> {
        let mut body = XMP_UUID.to_vec();
        body.extend_from_slice(PACKET.as_bytes());
        mp4_box(b"uuid", &body)
    }
    #[test]
    fn png_bytes_after_iend_survive_a_tag_write_and_the_structural_compare() {
        let mut with_tail = png_bytes();
        let clean_len = with_tail.len();
        with_tail.extend_from_slice(b"PK\x03\x04 an archive somebody glued on");
        assert_eq!(png_end(&with_tail), Some(clean_len));
        let tagged = set_xmp(&with_tail, PACKET).unwrap();
        assert!(tagged.ends_with(b"an archive somebody glued on"));
        assert_eq!(get_xmp(&tagged).unwrap().as_deref(), Some(PACKET));
        assert_eq!(strip_xmp(&tagged).unwrap(), strip_xmp(&with_tail).unwrap());
        let untagged = set_xmp(&tagged, "").unwrap();
        assert_eq!(
            untagged, with_tail,
            "removing the packet gives the original back, tail included"
        );
    }
    #[test]
    fn webp_bytes_past_the_riff_size_survive() {
        let mut with_tail = include_bytes!("../tests/fixtures/animated.webp").to_vec();
        with_tail.extend_from_slice(b"trailing junk!");
        let tagged = set_xmp(&with_tail, PACKET).unwrap();
        assert!(tagged.ends_with(b"trailing junk!"));
        assert_eq!(get_xmp(&tagged).unwrap().as_deref(), Some(PACKET));
        assert_eq!(strip_xmp(&tagged).unwrap(), strip_xmp(&with_tail).unwrap());
    }
    #[test]
    fn mp4_refuses_a_foreign_xmp_box_ahead_of_the_media_and_accepts_one_at_the_end() {
        let ftyp = mp4_box(b"ftyp", b"isom\0\0\0\0isom");
        let moov = mp4_box(b"moov", &[0; 16]);
        let mdat = mp4_box(b"mdat", &[1; 32]);
        let ahead: Vec<u8> = [ftyp.clone(), moov.clone(), xmp_box(), mdat.clone()].concat();
        let err = set_xmp(&ahead, PACKET).unwrap_err();
        assert!(err.contains("ahead of the media"), "{err}");
        let at_end: Vec<u8> = [ftyp.clone(), moov.clone(), mdat.clone(), xmp_box()].concat();
        let retagged = set_xmp(&at_end, PACKET).unwrap();
        assert_eq!(
            retagged, at_end,
            "our own box at the end is replaced in place"
        );
        let plain: Vec<u8> = [ftyp, moov, mdat].concat();
        assert_eq!(set_xmp(&plain, PACKET).unwrap(), at_end);
    }
}

#[cfg(test)]
mod gif_tests {
    use super::*;
    /// A 1×1 GIF: header without a global colour table, `blocks` before the image, then the trailer byte.
    fn gif(blocks: &[Vec<u8>], trailer: u8) -> Vec<u8> {
        let mut v = b"GIF89a\x01\x00\x01\x00\x00\x00\x00".to_vec();
        for b in blocks {
            v.extend_from_slice(b);
        }
        v.extend_from_slice(b"\x2C\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02\x44\x01\x00");
        v.push(trailer);
        v
    }
    /// An XMP application extension as a generic writer frames it: 255-byte sub-blocks, no magic trailer.
    fn framed(payload: &[u8]) -> Vec<u8> {
        let mut v = b"\x21\xFF\x0BXMP DataXMP".to_vec();
        for chunk in payload.chunks(255) {
            v.push(chunk.len() as u8);
            v.extend_from_slice(chunk);
        }
        v.push(0);
        v
    }
    fn blocks(b: &[u8]) -> usize {
        b.windows(11).filter(|w| *w == b"XMP DataXMP").count()
    }
    #[test]
    fn framed_blocks_fragments_many_copies_and_a_bad_trailer_byte() {
        let packet = crate::xmp::merge(None, &["cat".into()], &Default::default()).unwrap();
        let clean = gif(&[], 0x3B);
        // memetag's own form, and a generic writer's framed form, both read
        let ours = set_xmp(&clean, &packet).unwrap();
        assert_eq!(get_xmp(&ours).unwrap().as_deref(), Some(packet.as_str()));
        let generic = gif(&[framed(packet.as_bytes())], 0x3B);
        assert_eq!(get_xmp(&generic).unwrap().as_deref(), Some(packet.as_str()));
        assert_eq!(strip_xmp(&generic).unwrap(), clean);
        // a re-encoder's fragment is not a packet; a write replaces it with ours and removal leaves a clean file
        let fragment = gif(&[framed(b"?xp")], 0x3B);
        assert_eq!(get_xmp(&fragment).unwrap(), None);
        let tagged = set_xmp(&fragment, &packet).unwrap();
        assert_eq!(get_xmp(&tagged).unwrap().as_deref(), Some(packet.as_str()));
        assert_eq!(blocks(&tagged), 1);
        assert_eq!(set_xmp(&tagged, "").unwrap(), clean);
        // one packet per frame (Fireworks): the first readable one is the packet, a write leaves exactly one
        let many = gif(&vec![framed(packet.as_bytes()); 3], 0x3B);
        assert_eq!(get_xmp(&many).unwrap().as_deref(), Some(packet.as_str()));
        let one = set_xmp(&many, &packet).unwrap();
        assert_eq!(blocks(&one), 1);
        assert_eq!(strip_xmp(&many).unwrap(), strip_xmp(&one).unwrap());
        assert_eq!(strip_xmp(&one).unwrap(), clean);
        // `<` where the `;` trailer belongs: taggable, and the byte stays where it was
        let odd = gif(&[], 0x3C);
        assert_eq!(get_xmp(&odd).unwrap(), None);
        let t = set_xmp(&odd, &packet).unwrap();
        assert!(t.ends_with(b"\x3C"));
        assert_eq!(get_xmp(&t).unwrap().as_deref(), Some(packet.as_str()));
        assert_eq!(set_xmp(&t, "").unwrap(), odd);
        // a packet cut off inside its RDF is not mergeable either
        let cut = &packet[..packet.find("<rdf:Description").unwrap() + 30];
        let truncated = gif(&[framed(cut.as_bytes())], 0x3B);
        assert_eq!(get_xmp(&truncated).unwrap(), None);
        // Fireworks' trailer: 0x00 in the 0x3B slot; the packet reads and a write merges rather than replaces
        let mut fireworks = ours.clone();
        let at = fireworks
            .windows(3)
            .position(|w| w == [0x3C, 0x3B, 0x3A])
            .unwrap()
            + 1;
        fireworks[at] = 0;
        assert_eq!(
            get_xmp(&fireworks).unwrap().as_deref(),
            Some(packet.as_str())
        );
        assert_eq!(set_xmp(&fireworks, &packet).unwrap(), ours);
    }
}

#[cfg(test)]
mod avif_tests {
    use super::*;
    fn ftyp(brand: &[u8]) -> Vec<u8> {
        let mut b = vec![0, 0, 0, 20];
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(brand);
        b.extend_from_slice(&[0, 0, 0, 0, b'm', b'i', b'f', b'1']);
        b
    }
    #[test]
    fn a_still_avif_is_an_image_and_an_animated_one_stays_a_video() {
        assert_eq!(sniff(&ftyp(b"avif")), Kind::Avif);
        assert!(Kind::Avif.is_image());
        assert_eq!(sniff(&ftyp(b"avis")), Kind::Mp4);
        assert_eq!(sniff(&ftyp(b"isom")), Kind::Mp4);
    }
}
