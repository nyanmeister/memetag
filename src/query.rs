//! derpibooru-style search grammar.
//!   a, b            AND (also the word AND / &&)
//!   a || b          OR  (also the word OR)
//!   -a  !a  NOT a   NOT
//!   ( ... )         grouping;  AND binds tighter than OR, NOT tightest
//!   wild*card?      glob against the tag text (case-insensitive)
//!   ns:value        just a tag whose text contains a colon
//!   field queries:  format:gif  width.gt:1000  height.lte:500  size.gt:1000000  created_at.gt:2019
//!                   tag_count.lt:3  id:abc123  path:*reactions*  kind:video
//!   text.word:tea   whole words of the OCR text (t.w:tea); text:tea matches "instead"
//!   similar:<image> files whose perceptual hash is within 20 bits of that image's (a library path, a file, or clipboard); @N after it sets the bits
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Tag(String),
    ExactTag(String),
    Field { name: String, op: Op, value: String },
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    All,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Eq,
    Gt,
    Gte,
    Lt,
    Lte,
    Word,
}

const FIELDS: &[&str] = &[
    "format",
    "kind",
    "width",
    "height",
    "size",
    "created_at",
    "tagged_at",
    "id",
    "tag_count",
    "path",
    "source",
    "text",
    "similar",
];
// `folder:` is deliberately not a field: `folder:old` is the folder tag itself (one path component, exact, aliases
// and wildcards as for any tag). As a substring of the path it made `NOT folder:old` drop every file with "old"
// anywhere in its path — golden, bold, soldier (review, 2026-09-22). `path:` is the substring form.

/// `similar:` value → (image, max differing bits): a trailing `@N` sets the bits, else the default.
pub fn similar_spec(v: &str) -> (&str, u32) {
    match v.rsplit_once('@') {
        Some((img, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            (img, n.parse().unwrap_or(crate::similar::DEFAULT_DISTANCE))
        }
        _ => (v, crate::similar::DEFAULT_DISTANCE),
    }
}
/// Every image a query compares against, so the caller can hash them once before evaluating.
pub fn similar_terms(e: &Expr) -> Vec<String> {
    fn walk(e: &Expr, out: &mut Vec<String>) {
        match e {
            Expr::Field { name, value, .. } if name == "similar" => {
                let (img, _) = similar_spec(value);
                if !out.iter().any(|x| x == img) {
                    out.push(img.to_string());
                }
            }
            Expr::And(a, b) | Expr::Or(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            Expr::Not(a) => walk(a, out),
            _ => {}
        }
    }
    let mut out = vec![];
    walk(e, &mut out);
    out
}

pub fn parse(q: &str) -> Result<Expr, String> {
    let toks = lex(q)?;
    let mut p = Parser { toks, i: 0 };
    if p.toks.is_empty() {
        return Ok(Expr::All);
    }
    let e = p.or()?;
    if p.i != p.toks.len() {
        return Err(format!("unexpected {:?}", p.toks[p.i]));
    }
    Ok(e)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Term(String),
    QuotedTerm(String),
    And,
    Or,
    Not,
    LParen,
    RParen,
}

fn lex(q: &str) -> Result<Vec<Tok>, String> {
    let mut out = vec![];
    let mut cur = String::new();
    let cs: Vec<char> = q.chars().collect();
    let mut i = 0;
    let mut quoted = false;
    let flush = |cur: &mut String, quoted: &mut bool, out: &mut Vec<Tok>| {
        let t = cur.trim().to_string();
        if !t.is_empty() {
            out.push(if *quoted {
                Tok::QuotedTerm(t)
            } else {
                Tok::Term(t)
            });
        }
        cur.clear();
        *quoted = false;
    };
    while i < cs.len() {
        let c = cs[i];
        // Only standalone, unquoted words are operators. Preserve spaces inside
        // ordinary multiword tags and field values, including text:OR.
        if i == 0
            || cs[i - 1].is_whitespace()
            || matches!(cs[i - 1], '(' | ')' | ',' | '!' | '|' | '&')
        {
            let mut end = i;
            while end < cs.len() && cs[end].is_ascii_alphabetic() {
                end += 1;
            }
            if end > i
                && (end == cs.len()
                    || cs[end].is_whitespace()
                    || matches!(cs[end], '(' | ')' | ','))
            {
                let word: String = cs[i..end].iter().collect();
                if let Some(op) = word_operator(&word) {
                    flush(&mut cur, &mut quoted, &mut out);
                    out.push(op);
                    i = end;
                    continue;
                }
            }
        }
        match c {
            ',' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::And);
            }
            '|' if cs.get(i + 1) == Some(&'|') => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::Or);
                i += 1;
            }
            '&' if cs.get(i + 1) == Some(&'&') => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::And);
                i += 1;
            }
            '(' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::LParen);
            }
            ')' => {
                flush(&mut cur, &mut quoted, &mut out);
                out.push(Tok::RParen);
            }
            '-' | '!' if cur.trim().is_empty() => {
                out.push(Tok::Not);
            }
            '"' => {
                if cur.trim().is_empty() {
                    quoted = true;
                }
                let mut j = i + 1;
                while j < cs.len() && cs[j] != '"' {
                    if cs[j] == '\\' && cs.get(j + 1).is_some_and(|c| matches!(c, '"' | '\\')) {
                        j += 1;
                    }
                    cur.push(cs[j]);
                    j += 1;
                }
                if j == cs.len() {
                    return Err("unclosed quote".into());
                }
                i = j;
                // The closing quote ends the term when a space or an operator follows: `"a" b` is two terms, not
                // the tag `a b` (review, 2026-09-22). A suffix glued on, as in `similar:"a, b.png"@20`, still belongs.
                if cs
                    .get(j + 1)
                    .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | ')' | ',' | '"'))
                {
                    flush(&mut cur, &mut quoted, &mut out);
                }
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut quoted, &mut out);
    Ok(out)
}
fn word_operator(t: &str) -> Option<Tok> {
    match t.to_ascii_uppercase().as_str() {
        "AND" => Some(Tok::And),
        "OR" => Some(Tok::Or),
        "NOT" => Some(Tok::Not),
        _ => None,
    }
}

/// Byte range of the tag being edited, with a decoded prefix for completion.
/// Operators and field values are not completion targets. Character indices
/// come from egui; converting through char_indices keeps Unicode edits safe.
pub fn completion(q: &str, caret: usize) -> Option<(std::ops::Range<usize>, String)> {
    let chars: Vec<char> = q.chars().collect();
    let mut offsets: Vec<_> = q.char_indices().map(|(i, _)| i).collect();
    offsets.push(q.len());
    if caret > chars.len() {
        return None;
    }
    let (mut start, mut i) = (0, 0);
    let mut spans = vec![];
    while i < chars.len() {
        if chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && chars.get(i + 1).is_some_and(|c| matches!(c, '"' | '\\')) {
                    i += 1;
                }
                i += 1;
            }
            if i < chars.len() {
                i += 1;
            }
            continue;
        }
        let mut separator = if matches!(chars[i], ',' | '(' | ')') {
            1
        } else if (chars[i] == '|' || chars[i] == '&') && chars.get(i + 1) == Some(&chars[i]) {
            2
        } else {
            0
        };
        if matches!(chars[i], '-' | '!') && chars[start..i].iter().all(|c| c.is_whitespace()) {
            separator = 1;
        }
        if i == 0
            || chars[i - 1].is_whitespace()
            || matches!(chars[i - 1], '(' | ')' | ',' | '!' | '|' | '&')
        {
            let mut end = i;
            while end < chars.len() && chars[end].is_ascii_alphabetic() {
                end += 1;
            }
            if end > i
                && (end == chars.len()
                    || chars[end].is_whitespace()
                    || matches!(chars[end], '(' | ')' | ','))
                && word_operator(&chars[i..end].iter().collect::<String>()).is_some()
            {
                separator = end - i;
            }
        }
        if separator > 0 {
            spans.push((start, i));
            i += separator;
            start = i;
        } else {
            i += 1;
        }
    }
    spans.push((start, chars.len()));
    for (mut start, mut end) in spans {
        while start < end && chars[start].is_whitespace() {
            start += 1;
        }
        while end > start && chars[end - 1].is_whitespace() {
            end -= 1;
        }
        if start == end || caret < start || caret > end {
            continue;
        }
        let raw = &q[offsets[start]..offsets[end]];
        if !raw.starts_with('"') && matches!(term(raw), Expr::Field { .. }) {
            return None;
        }
        let prefix = &q[offsets[start]..offsets[caret]];
        let mut prefix_chars = prefix.chars().peekable();
        let mut decoded = String::new();
        while let Some(c) = prefix_chars.next() {
            if c == '"' {
                continue;
            }
            if c == '\\' && prefix_chars.peek().is_some_and(|c| matches!(c, '"' | '\\')) {
                decoded.push(prefix_chars.next().unwrap());
            } else {
                decoded.push(c);
            }
        }
        if decoded.trim().is_empty() {
            return None;
        }
        return Some((offsets[start]..offsets[end], decoded));
    }
    None
}

/// Search syntax credit: https://derpibooru.org/pages/search_syntax
/// The in-app help table: (example, what it does). Sections are ("", heading).
pub const SYNTAX_HELP: &[(&str, &str)] = &[
    ("", "Tags"),
    ("this is fine", "a tag; multiword tags stay one term"),
    ("\"width:1920\"", "exact tag; quotes keep punctuation, wildcards and operator words literal"),
    ("wild*card?", "glob against tag text (* any run, ? one character)"),
    ("character:reimu", "an unknown field is just a tag containing a colon"),
    ("", "Combining"),
    ("a, b   a AND b   a && b", "both"),
    ("a || b   a OR b", "either"),
    ("-a   !a   NOT a", "exclude"),
    ("(a OR b) AND c", "grouping; NOT binds tightest, then AND, then OR"),
    ("", "OCR text"),
    ("text:tea   t:tea", "text contains \"tea\" anywhere, so it also finds \"instead\" and \"team\""),
    ("text.word:tea   t.w:tea", "whole words only: \"tea.\" and \"TEA\" match, \"instead\" and \"team\" do not"),
    ("t.w:\"green tea\"", "consecutive whole words; punctuation and line breaks between them are ignored"),
    ("t.w:te*", "wildcards apply per word: tea and team, not instead"),
    ("t:\"this AND that, or\"", "quote a phrase containing operator words, commas or parentheses"),
    ("t:*Caesar*", "unquoted wildcards match against the whole text"),
    ("", "File fields"),
    ("format:gif   kind:video", "container format (jpg = jpeg, webm = mkv, apng = animated png) or kind: image, video, other"),
    ("folder:reactions", "a folder tag: one component of the path, whole; folder:react* for a wildcard"),
    ("path:*react*", "any part of the path, case-insensitive"),
    ("id:abc123", "id prefix"),
    ("width.gt:1000   height.lte:500", "compare with .gt .gte .lt .lte, or plain : for equal; videos and undecodable files have no dimensions and never match"),
    ("size.gt:1.5m   tag_count.lt:3", "sizes take k, m, g suffixes"),
    ("created_at:2019   tagged_at.gt:2019-07-14", "a year, month or day, in UTC; .gt means after it, .lt before it"),
    ("", "Image search"),
    ("similar:\"Old/cat (1).jpg\"", "files that look like one already here: a re-encode, another size, a captioned or lightly cropped copy. The path is the library path; the Similar button on a tile writes this. Quote it when it holds parentheses or commas"),
    ("similar:clipboard", "the image on the clipboard. The camera button writes this and reads the clipboard afresh; typed by hand, the clipboard is read once per search"),
    ("similar:~/Downloads/meme.png", "a file on this machine: ~ expands, and JPEG, PNG, GIF, WebP and BMP work (animations by their first frame)"),
    ("similar:clipboard@30", "loosen the match: differing hash bits allowed, 20 unless given, out of 256. Real copies sit under 12, unrelated images past 100. A name like icon@2x.png keeps its @"),
    ("similar:clipboard AND NOT folder:old", "combines with everything else; several similar: terms are fine. Results come closest first, so Enter copies the best match"),
    ("similar:/nope.png", "a red line under the search field: the value is neither a library path nor a readable image, the clipboard holds no image, or the image is flat (a solid colour, a blank page) with nothing to match on"),
    ("", "Examples"),
    ("t.w:tea AND NOT t.w:coffee", "text mentions the word tea and never the word coffee"),
    ("(t.w:cat OR t.w:dog) AND format:png", "any PNG whose text mentions a cat or a dog as a whole word"),
    ("folder:reactions AND NOT format:gif", "everything under a reactions folder except the GIFs"),
];
/// Quote completions so punctuation, wildcards and reserved namespaces remain
/// literal tags. Field searches use an unquoted prefix, e.g. text:"a phrase".
pub fn quote_tag(tag: &str) -> String {
    format!("\"{}\"", tag.replace('\\', "\\\\").replace('"', "\\\""))
}

struct Parser {
    toks: Vec<Tok>,
    i: usize,
}
impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }
    fn or(&mut self) -> Result<Expr, String> {
        let mut l = self.and()?;
        while self.peek() == Some(&Tok::Or) {
            self.i += 1;
            let r = self.and()?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn and(&mut self) -> Result<Expr, String> {
        let mut l = self.not()?;
        loop {
            match self.peek() {
                Some(Tok::And) => {
                    self.i += 1;
                }
                Some(Tok::Term(_))
                | Some(Tok::QuotedTerm(_))
                | Some(Tok::Not)
                | Some(Tok::LParen) => {}
                _ => break,
            } // juxtaposition = AND
            let r = self.not()?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        Ok(l)
    }
    fn not(&mut self) -> Result<Expr, String> {
        if self.peek() == Some(&Tok::Not) {
            self.i += 1;
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.atom()
    }
    fn atom(&mut self) -> Result<Expr, String> {
        match self.toks.get(self.i).cloned() {
            Some(Tok::LParen) => {
                self.i += 1;
                let e = self.or()?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err("missing )".into());
                }
                self.i += 1;
                Ok(e)
            }
            Some(Tok::Term(t)) => {
                self.i += 1;
                Ok(term(&t))
            }
            Some(Tok::QuotedTerm(t)) => {
                self.i += 1;
                Ok(Expr::ExactTag(t.to_ascii_lowercase()))
            }
            other => Err(format!("expected a term, found {other:?}")),
        }
    }
}
fn term(t: &str) -> Expr {
    if let Some((head, value)) = t.split_once(':') {
        let (name, op) = match head
            .rsplit_once('.')
            .map(|(n, o)| (n, o.to_ascii_lowercase()))
            .as_ref()
            .map(|(n, o)| (*n, o.as_str()))
        {
            Some((n, "gt")) => (n, Op::Gt),
            Some((n, "gte")) => (n, Op::Gte),
            Some((n, "lt")) => (n, Op::Lt),
            Some((n, "lte")) => (n, Op::Lte),
            Some((n, "word" | "w")) => (n, Op::Word),
            _ => (head, Op::Eq),
        };
        let name = if name.eq_ignore_ascii_case("t") {
            "text"
        } else {
            name
        };
        // .word only means something for OCR text; width.word:x stays a literal tag like any unknown field.
        if FIELDS.contains(&name.to_ascii_lowercase().as_str())
            && (op != Op::Word || name.eq_ignore_ascii_case("text"))
        {
            return Expr::Field {
                name: name.to_ascii_lowercase(),
                op,
                value: value.trim().to_string(),
            };
        }
    }
    Expr::Tag(t.to_ascii_lowercase())
}

/// Everything the evaluator may ask about one file.
/// `similar` answers (query image, this file's path) with their hash distance, None when either has no hash.
pub struct Item<'a> {
    pub id: &'a str,
    pub path: &'a str,
    pub format: &'a str,
    pub kind: &'a str,
    pub width: i64,
    pub height: i64,
    pub size: i64,
    pub created_at: f64,
    pub tagged_at: f64,
    pub tags: &'a BTreeSet<String>,
    pub xmp_tag_count: i64,
    pub text: &'a str,
    pub similar: &'a dyn Fn(&str, &str) -> Option<u32>,
}
impl<'a> Item<'a> {
    /// The one place an indexed file becomes a query item; `similar` is the image-search resolver (`&|_, _| None` when the query has no similar: term).
    pub fn of(
        f: &'a crate::index::FileRow,
        similar: &'a dyn Fn(&str, &str) -> Option<u32>,
    ) -> Item<'a> {
        Item {
            id: &f.id,
            path: &f.path,
            format: &f.format,
            kind: &f.kind,
            width: f.width,
            height: f.height,
            size: f.size,
            created_at: f.created_at,
            tagged_at: f.tagged_at,
            tags: &f.tags,
            xmp_tag_count: f.xmp_tag_count,
            text: &f.text,
            similar,
        }
    }
}

pub fn eval(e: &Expr, it: &Item, alias: &dyn Fn(&str) -> String) -> bool {
    match e {
        Expr::All => true,
        Expr::And(a, b) => eval(a, it, alias) && eval(b, it, alias),
        Expr::Or(a, b) => eval(a, it, alias) || eval(b, it, alias),
        Expr::Not(a) => !eval(a, it, alias),
        Expr::Tag(t) => {
            let t = alias(t);
            if t.contains('*') || t.contains('?') {
                it.tags.iter().any(|x| glob(&t, x))
            } else {
                it.tags.contains(&t)
            }
        }
        Expr::ExactTag(t) => it.tags.contains(&alias(t)),
        Expr::Field { name, op, value } => field(name, *op, value, it),
    }
}
fn cmp_num(a: f64, op: Op, b: f64) -> bool {
    match op {
        Op::Eq => (a - b).abs() < 1e-9,
        Op::Gt => a > b,
        Op::Gte => a >= b,
        Op::Lt => a < b,
        Op::Lte => a <= b,
        Op::Word => false,
    }
}
fn field(name: &str, op: Op, v: &str, it: &Item) -> bool {
    // 1000, 2k, 1.5m, 10mb, 3g → a multiplier on the number (the old replace-'m'-with-zeros turned 1.5m into 1.5)
    let num = |s: &str| -> Option<f64> {
        let t = s.trim().to_ascii_lowercase();
        let t = t.trim_end_matches('b');
        let (body, mult) = match t.chars().last() {
            Some('k') => (&t[..t.len() - 1], 1e3),
            Some('m') => (&t[..t.len() - 1], 1e6),
            Some('g') => (&t[..t.len() - 1], 1e9),
            _ => (t, 1.0),
        };
        body.trim().parse::<f64>().ok().map(|v| v * mult)
    };
    // an unknown dimension is stored as 0 (videos, undecodable files): it compares with nothing but 0 itself
    let dim = |d: i64| num(v).is_some_and(|n| (d > 0 || n == 0.0) && cmp_num(d as f64, op, n));
    match name {
        "format" => {
            it.format.eq_ignore_ascii_case(v)
                || (v.eq_ignore_ascii_case("jpg") && it.format == "jpeg")
                || (v.eq_ignore_ascii_case("webm") && it.format == "mkv")
        }
        "kind" => it.kind.eq_ignore_ascii_case(v),
        "id" => it.id.starts_with(v),
        "source" => it
            .path
            .split('/')
            .next()
            .is_some_and(|id| id.eq_ignore_ascii_case(v)),
        "path" => glob(
            &format!("*{}*", v.to_ascii_lowercase()),
            &it.path.to_ascii_lowercase(),
        ),
        "text" if op == Op::Word => word_match(it.text, v),
        "text" => {
            // one pass, one copy: lowercased for every alphabet (memes shout in Cyrillic too), line breaks as spaces
            let t: String = it
                .text
                .chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .flat_map(char::to_lowercase)
                .collect();
            let v = v.to_lowercase();
            if v.contains('*') || v.contains('?') {
                glob(&v, &t)
            } else {
                t.contains(&v)
            }
        }
        "width" => dim(it.width),
        "height" => dim(it.height),
        "size" => num(v).is_some_and(|n| cmp_num(it.size as f64, op, n)),
        "tag_count" => num(v).is_some_and(|n| cmp_num(it.xmp_tag_count as f64, op, n)),
        "created_at" | "tagged_at" => {
            let t = if name == "created_at" {
                it.created_at
            } else {
                it.tagged_at
            };
            match parse_date(v) {
                Some((lo, hi)) => match op {
                    Op::Eq => t >= lo && t < hi,
                    Op::Gt => t >= hi,
                    Op::Gte => t >= lo,
                    Op::Lt => t < lo,
                    Op::Lte => t < hi,
                    Op::Word => false,
                },
                None => false,
            }
        }
        "similar" => {
            let (img, max) = similar_spec(v);
            (it.similar)(img, it.path).is_some_and(|d| d <= max)
        }
        _ => false,
    }
}
/// text.word: the value's words must appear as consecutive whole words of the
/// text. Words are runs of alphanumerics, so punctuation and line breaks around
/// them never matter: `tea` matches "tea." and "TEA\nparty" but not "instead" or
/// "team". Wildcards apply per word: `te*` matches "tea" and "team", not "instead".
/// Runs per file on every keystroke of the grid, so it borrows: no lowercased copy, no owned tokens.
fn word_match(text: &str, v: &str) -> bool {
    let wild = v.contains('*') || v.contains('?');
    fn split(s: &str) -> impl Iterator<Item = &str> {
        s.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
    }
    let words: Vec<&str> = if wild {
        v.split_whitespace().collect()
    } else {
        split(v).collect()
    };
    if words.is_empty() {
        return false;
    }
    let toks: Vec<&str> = split(text).collect();
    toks.windows(words.len()).any(|w| {
        w.iter()
            .zip(&words)
            .all(|(t, p)| if wild { glob(p, t) } else { same_word(t, p) })
    })
}
/// Equal ignoring case in every alphabet, without an allocation.
fn same_word(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}
/// "2019" → the whole year, "2019-07" → the month, "2019-07-14" → the day; returns [lo, hi) as unix seconds.
fn parse_date(v: &str) -> Option<(f64, f64)> {
    let p: Vec<i64> = v
        .split(['-', '/', ':'])
        .filter_map(|s| s.parse().ok())
        .collect();
    let (y, m, d) = (*p.first()?, p.get(1).copied(), p.get(2).copied());
    let days = |y: i64, m: i64, d: i64| -> i64 {
        // days since epoch (proleptic Gregorian), civil-from-days inverse
        let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * m + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146097 + doe - 719468
    };
    let lo = match (m, d) {
        (Some(m), Some(d)) => days(y, m, d),
        (Some(m), None) => days(y, m, 1),
        _ => days(y, 1, 1),
    };
    let hi = match (m, d) {
        (Some(m), Some(d)) => days(y, m, d) + 1,
        (Some(m), None) => {
            if m == 12 {
                days(y + 1, 1, 1)
            } else {
                days(y, m + 1, 1)
            }
        }
        _ => days(y + 1, 1, 1),
    };
    Some(((lo * 86400) as f64, (hi * 86400) as f64))
}
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    let same = |a: char, b: char| a.to_lowercase().eq(b.to_lowercase());
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || same(p[pi], t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completion_preserves_neighbors_and_unicode() {
        for (q, caret, expected, prefix) in [
            ("cat, -am", 8, "am", "am"),
            ("cat OR (artist:am AND dog)", 17, "artist:am", "artist:am"),
            (
                "café, \"rock OR r\" AND dog",
                16,
                "\"rock OR r\"",
                "rock OR r",
            ),
            ("one, amber cat, two", 8, "amber cat", "amb"),
            ("\"rock, OR r", 11, "\"rock, OR r", "rock, OR r"),
        ] {
            let (range, got) = completion(q, caret).unwrap();
            assert_eq!(&q[range.clone()], expected, "{q}");
            assert_eq!(got, prefix, "{q}");
            let mut result = q.to_string();
            result.replace_range(range, &quote_tag("new (tag)"));
            assert!(parse(&result).is_ok(), "{result}");
        }
        for q in [
            "t:am",
            "text:\"am",
            "width.gt:100",
            "cat AND ",
            "cat OR",
            "",
            " ",
        ] {
            assert!(completion(q, q.chars().count()).is_none(), "{q}");
        }
    }
    #[test]
    fn completed_tags_are_literal_even_with_reserved_syntax() {
        for tag in [
            "width:1920",
            "format:gif",
            "what?",
            "a*b",
            "rock OR roll",
            "hi, there",
            "-negative",
            "artist:a",
            "café",
            "a \\\"quote\\\"",
            "a\\b",
        ] {
            assert_eq!(
                parse(&quote_tag(tag)).unwrap(),
                Expr::ExactTag(tag.to_ascii_lowercase())
            );
        }
        assert!(matches!(
            parse("text:\"rock OR roll\"").unwrap(),
            Expr::Field { .. }
        ));
    }
    #[test]
    fn short_text_alias() {
        for (short, long) in [
            ("t:cat", "text:cat"),
            ("T:*Caesar*", "text:*Caesar*"),
            (
                "t:\"rock AND roll\" OR NOT t:dog",
                "text:\"rock AND roll\" OR NOT text:dog",
            ),
        ] {
            assert_eq!(parse(short).unwrap(), parse(long).unwrap());
        }
    }
    #[test]
    fn whole_word_text() {
        for (short, long) in [
            ("t.w:tea", "text.word:tea"),
            ("T.W:tea", "text.word:tea"),
            (
                "t.w:\"green tea\" OR NOT t.w:te*",
                "text.word:\"green tea\" OR NOT text.word:te*",
            ),
        ] {
            assert_eq!(parse(short).unwrap(), parse(long).unwrap(), "{short}");
        }
        assert_eq!(
            parse("t.w:tea").unwrap(),
            Expr::Field {
                name: "text".into(),
                op: Op::Word,
                value: "tea".into()
            }
        );
        assert_eq!(
            parse("width.word:tea").unwrap(),
            Expr::Tag("width.word:tea".into())
        );
        let tags = BTreeSet::new();
        let item = |text: &'static str| Item {
            id: "x",
            path: "p",
            format: "png",
            kind: "image",
            width: 1,
            height: 1,
            size: 1,
            created_at: 0.0,
            tagged_at: 0.0,
            tags: &tags,
            xmp_tag_count: 0,
            text,
            similar: &|_, _| None,
        };
        for (text, v, expect) in [
            ("instead of coffee", "tea", false),
            ("the team", "tea", false),
            ("Tea", "tea", true),
            ("tea", "tea", true),
            ("cup of tea.", "tea", true),
            ("TEA\nPARTY", "tea", true),
            ("(tea)", "tea", true),
            ("", "tea", false),
            ("green\ntea, please", "green tea", true),
            ("green, tea", "green tea", true),
            ("green teapot", "green tea", false),
            ("don't panic", "don't", true),
            ("don't panic", "dont", false),
            ("tea and team", "te*", true),
            ("instead", "te*", false),
            ("instead", "*tea*", true),
            ("green tea", "gr* t?a", true),
            ("green teapot", "gr* t?a", false),
            ("tea", "", false),
            ("tea", " . ", false),
            ("ХУЙ ВОЙНЕ", "хуй", true),
            ("CAFÉ AU LAIT", "café", true),
            ("Ärger", "ärg*", true),
        ] {
            assert_eq!(
                field("text", Op::Word, v, &item(text)),
                expect,
                "text.word:{v:?} on {text:?}"
            );
        }
        assert!(field("text", Op::Eq, "tea", &item("instead")));
        assert!(field("text", Op::Eq, "хуй", &item("ХУЙ ВОЙНЕ")));
        assert!(field("text", Op::Eq, "*café*", &item("UN CAFÉ")));
        assert!(!field("width", Op::Word, "1", &item("")));
    }
    #[test]
    fn folder_is_the_tag_and_unknown_dimensions_match_nothing() {
        let tags: BTreeSet<String> = ["folder:old", "folder:reactions", "cat"]
            .into_iter()
            .map(String::from)
            .collect();
        let none = BTreeSet::new();
        let item = |tags: &'static BTreeSet<String>, path: &'static str, w: i64| Item {
            id: "x",
            path,
            format: "mkv",
            kind: "video",
            width: w,
            height: w,
            size: 1,
            created_at: 0.0,
            tagged_at: 0.0,
            tags,
            xmp_tag_count: 0,
            text: "",
            similar: &|_, _| None,
        };
        let tags: &'static BTreeSet<String> = Box::leak(Box::new(tags));
        let none: &'static BTreeSet<String> = Box::leak(Box::new(none));
        let alias = |t: &str| t.to_string();
        let golden = item(none, "Reactions/golden.png", 0);
        let old = item(tags, "Old/x.png", 640);
        assert_eq!(parse("folder:old").unwrap(), Expr::Tag("folder:old".into()));
        assert!(eval(&parse("folder:old").unwrap(), &old, &alias));
        assert!(!eval(&parse("folder:old").unwrap(), &golden, &alias));
        assert!(eval(&parse("NOT folder:old").unwrap(), &golden, &alias));
        assert!(eval(&parse("folder:react*").unwrap(), &old, &alias));
        assert!(eval(&parse("path:*old*").unwrap(), &golden, &alias));
        assert!(eval(&parse("format:webm").unwrap(), &old, &alias));
        assert!(!eval(&parse("width.lt:500").unwrap(), &golden, &alias));
        assert!(!eval(&parse("width.lt:500").unwrap(), &old, &alias));
        assert!(eval(&parse("width.lt:700").unwrap(), &old, &alias));
        assert!(eval(&parse("width:0").unwrap(), &golden, &alias));
    }
    #[test]
    fn word_operators_and_literals() {
        for (words, symbols) in [
            ("a OR b", "a || b"),
            ("a and b", "a, b"),
            ("NOT a", "-a"),
            ("a OR b AND NOT c", "a || b, -c"),
            ("NOT(a OR b)", "-(a || b)"),
        ] {
            assert_eq!(parse(words).unwrap(), parse(symbols).unwrap(), "{words}");
        }
        for literal in [
            "this is fine",
            "candy",
            "orange",
            "NOTHING",
            "\"AND\"",
            "\"rock OR roll\"",
        ] {
            let tag = literal.trim_matches('"').to_ascii_lowercase();
            assert_eq!(
                parse(literal).unwrap(),
                if literal.starts_with('"') {
                    Expr::ExactTag(tag)
                } else {
                    Expr::Tag(tag)
                }
            );
        }
        assert_eq!(
            parse("text:OR").unwrap(),
            Expr::Field {
                name: "text".into(),
                op: Op::Eq,
                value: "OR".into()
            }
        );
        assert_eq!(
            parse("text:\"this AND that, OR not\"").unwrap(),
            Expr::Field {
                name: "text".into(),
                op: Op::Eq,
                value: "this AND that, OR not".into()
            }
        );
        for bad in ["a AND", "a OR", "NOT", "a AND OR b", "\"unclosed"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn grammar() {
        assert_eq!(
            parse("a, b").unwrap(),
            Expr::And(
                Box::new(Expr::Tag("a".into())),
                Box::new(Expr::Tag("b".into()))
            )
        );
        assert!(matches!(parse("a || -b").unwrap(), Expr::Or(_, _)));
        assert!(matches!(parse("(a || b), c").unwrap(), Expr::And(_, _)));
        assert!(matches!(
            parse("width.gt:1000").unwrap(),
            Expr::Field { op: Op::Gt, .. }
        ));
        assert!(matches!(parse("character:reimu").unwrap(), Expr::Tag(_)));
        assert!(glob("this*fine", "this is fine"));
        assert!(!glob("this*fine", "this is fin"));
        let (lo, hi) = parse_date("2019").unwrap();
        assert_eq!(lo, 1546300800.0);
        assert_eq!(hi, 1577836800.0);
    }
    #[test]
    fn a_closing_quote_ends_the_term() {
        let and = |a: Expr, b: Expr| Expr::And(Box::new(a), Box::new(b));
        assert_eq!(
            parse("\"a\" b").unwrap(),
            and(Expr::ExactTag("a".into()), Expr::Tag("b".into()))
        );
        assert_eq!(
            parse("\"a\" \"b\"").unwrap(),
            and(Expr::ExactTag("a".into()), Expr::ExactTag("b".into()))
        );
        assert_eq!(
            parse("\"a\"\"b\"").unwrap(),
            and(Expr::ExactTag("a".into()), Expr::ExactTag("b".into()))
        );
    }
    #[test]
    fn similar_field() {
        assert_eq!(
            parse("similar:~/Downloads/some meme.png").unwrap(),
            Expr::Field {
                name: "similar".into(),
                op: Op::Eq,
                value: "~/Downloads/some meme.png".into()
            }
        ); // spaces stay; parentheses and commas need the quoted form
        assert_eq!(
            parse("similar:\"Old/a, b.png\"@20").unwrap(),
            Expr::Field {
                name: "similar".into(),
                op: Op::Eq,
                value: "Old/a, b.png@20".into()
            }
        );
        assert_eq!(similar_spec("Old/a.png@20"), ("Old/a.png", 20));
        assert_eq!(
            similar_spec("Old/a.png"),
            ("Old/a.png", crate::similar::DEFAULT_DISTANCE)
        );
        assert_eq!(
            similar_spec("Old/a@2x.png"),
            ("Old/a@2x.png", crate::similar::DEFAULT_DISTANCE)
        );
        assert_eq!(
            similar_terms(&parse("similar:a@9 AND (b OR NOT similar:c) OR similar:a").unwrap()),
            vec!["a".to_string(), "c".to_string()]
        );
        let tags = BTreeSet::new();
        let dist = |img: &str, path: &str| {
            if img == "q" && path == "near" {
                Some(5)
            } else if img == "q" {
                Some(30)
            } else {
                None
            }
        };
        let it = |path: &'static str| Item {
            id: "x",
            path,
            format: "png",
            kind: "image",
            width: 1,
            height: 1,
            size: 1,
            created_at: 0.0,
            tagged_at: 0.0,
            tags: &tags,
            xmp_tag_count: 0,
            text: "",
            similar: &dist,
        };
        assert!(field("similar", Op::Eq, "q", &it("near")));
        assert!(!field("similar", Op::Eq, "q", &it("far")));
        assert!(field("similar", Op::Eq, "q@30", &it("far")));
        assert!(!field("similar", Op::Eq, "other", &it("near")));
    }
    #[test]
    fn size_suffixes() {
        let tags = BTreeSet::new();
        let it = Item {
            id: "x",
            path: "p",
            format: "png",
            kind: "image",
            width: 1920,
            height: 1080,
            size: 1_600_000,
            created_at: 0.0,
            tagged_at: 0.0,
            tags: &tags,
            xmp_tag_count: 0,
            text: "",
            similar: &|_, _| None,
        };
        for (v, expect) in [
            ("1.5m", true),
            ("1.5mb", true),
            ("2m", false),
            ("1500k", true),
            ("1600000", false),
            ("0.001g", true),
            ("1000000", true),
        ] {
            assert_eq!(field("size", Op::Gt, v, &it), expect, "size.gt:{v}");
        }
        assert!(field("width", Op::Gte, "1.92k", &it));
    }
}
