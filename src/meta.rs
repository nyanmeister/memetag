//! Action tags. A `meta:… request` tag asks memetag to do something to the file on the next `memetag meta` pass,
//! and the pass turns it into the matching done-tag in the same journaled write, so a request only changes when the
//! work landed; on failure it stays and the reason is printed. Request tags are almost never on a file, and the
//! autocomplete lists them even when no image carries the request tag yet.
//!
//! Machine output goes into the file's OCR text as a block under a header (`English (machine):`, `Speech (machine):`,
//! `Description (machine):`), after a blank line; a rerun replaces its own block. Reviewed text keeps its mark and the
//! block is added under it. Action rules: write straight in, no preview; request → done-tag, never
//! a plain erase; check for an audio track before anything is sent to whisper.
use crate::ollama::generate;
use crate::{index::Db, Cfg};
use rusqlite::params;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub struct Action {
    pub request: &'static str,
    pub done: &'static str,
    pub what: &'static str,
}
pub const ACTIONS: &[Action] = &[
    Action {
        request: "meta:translation request",
        done: "meta:translated",
        what: "the text is translated to English (line for line, literally) under \"English (machine):\"",
    },
    Action {
        request: "meta:reocr request",
        done: "meta:reocr done",
        what: "the image is read again with the current OCR model, replacing the machine text (reviewed text is kept; existing blocks stay)",
    },
    Action {
        request: "meta:speech request",
        done: "meta:speech transcribed",
        what: "a video's audio track is transcribed (config.toml speech_command, e.g. whisper-cli) under \"Speech (machine):\"; a file without an audio track keeps the tag",
    },
    Action {
        request: "meta:describe request",
        done: "meta:described",
        what: "the vision model describes the picture in a few plain sentences under \"Description (machine):\", so wordless images are searchable",
    },
];
pub const TRANSLATE: &str = "meta:translation request";
const ENGLISH: &str = "English (machine):";
const SPEECH: &str = "Speech (machine):";
const DESCRIPTION: &str = "Description (machine):";
const HEADERS: [&str; 3] = [ENGLISH, SPEECH, DESCRIPTION];

/// The text before any machine block, and the blocks after it, as they are.
pub fn split_blocks(text: &str) -> (&str, &str) {
    let first = HEADERS.iter().filter_map(|h| text.find(h)).min();
    match first {
        Some(i) => (text[..i].trim_end(), &text[i..]),
        None => (text.trim_end(), ""),
    }
}
/// `text` with the block under `header` replaced (or added at the end), a blank line before it.
pub fn with_block(text: &str, header: &str, body: &str) -> String {
    let mut kept = String::new();
    let mut rest = text;
    while let Some(i) = rest.find(header) {
        kept.push_str(&rest[..i]);
        let after = &rest[i + header.len()..];
        let end = HEADERS
            .iter()
            .filter(|h| **h != header)
            .filter_map(|h| after.find(h))
            .min()
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    kept.push_str(rest);
    let base = kept.trim_end();
    if base.is_empty() {
        format!("{header}\n{}", body.trim())
    } else {
        format!("{base}\n\n{header}\n{}", body.trim())
    }
}
struct Opts {
    limit: usize,
    dry: bool,
    only: Option<String>,
}
fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        limit: 0,
        dry: false,
        only: None,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dry-run" => o.dry = true,
            "--limit" => {
                o.limit = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--limit needs a number")?;
                i += 1;
            }
            "--only" => {
                o.only = Some(
                    args.get(i + 1)
                        .cloned()
                        .ok_or("--only needs a request tag")?,
                );
                i += 1;
            }
            x => return Err(format!("meta: unknown flag {x}")),
        }
        i += 1;
    }
    Ok(o)
}

/// Does the file carry an audio stream? Check before launching transcription.
fn has_audio(path: &std::path::Path) -> Result<bool, String> {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).contains("audio"))
}
/// 16 kHz mono WAV of the audio track, in the temp dir; the caller removes it.
fn extract_audio(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let wav = std::env::temp_dir().join(format!("memetag-speech-{}.wav", std::process::id()));
    let st = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "wav"])
        .arg(&wav)
        .status()
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if !st.success() {
        let _ = std::fs::remove_file(&wav);
        return Err("ffmpeg could not extract the audio".into());
    }
    Ok(wav)
}
fn transcribe(c: &Cfg, wav: &std::path::Path) -> Result<String, String> {
    let cmd = c.speech_command.replace("{wav}", &wav.to_string_lossy());
    let out = std::process::Command::new("sh")
        .args(["-c", &cmd])
        .output()
        .map_err(|e| format!("speech_command: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .last()
            .unwrap_or(
                "no message; is whisper-cli on the PATH and the model where speech_command says?",
            );
        return Err(format!("speech_command failed ({}): {last}", out.status));
    }
    let text: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok(text.join("\n"))
}

struct Ctx<'a> {
    c: &'a Cfg,
    db: &'a Db,
    agent: ureq::Agent,
    url: String,
}

fn files_with(db: &Db, tag: &str) -> Result<Vec<String>, String> {
    let mut st = db
        .conn
        .prepare("SELECT DISTINCT path FROM tags WHERE tag=?1 AND source='xmp' ORDER BY path")
        .map_err(|e| e.to_string())?;
    let v = st
        .query_map(params![tag], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(v)
}

/// The file's own text (else the index's machine OCR), and whether it is reviewed.
fn current_text(x: &Ctx, rel: &str, parsed: &crate::xmp::Parsed) -> (String, bool) {
    let manual = parsed.fields.get("textSource").map(String::as_str) == Some("manual");
    let mut text = parsed.fields.get("text").cloned().unwrap_or_default();
    if text.trim().is_empty() {
        text =
            x.db.conn
                .query_row("SELECT body FROM text WHERE path=?1", params![rel], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap_or_default();
    }
    (text, manual)
}

fn vision_model(c: &Cfg) -> Result<&str, String> {
    c.ocr_model.as_deref().ok_or(
        "reading the image needs an ollama vision model; set ocr_model in config.toml".into(),
    )
}

/// What one action does to one file: the new text field (None = leave it), plus index writes it wants afterwards.
fn perform(
    x: &Ctx,
    action: &Action,
    rel: &str,
    row: &crate::index::FileRow,
    parsed: &crate::xmp::Parsed,
    before: &[u8],
) -> Result<Option<String>, String> {
    let c = x.c;
    let abs = c.file_path(rel)?;
    let (text, manual) = current_text(x, rel, parsed);
    match action.request {
        TRANSLATE => {
            if row.kind != "image" {
                return Err(format!("{} is not an image; the tag stays", row.kind));
            }
            let model = vision_model(c)?;
            let (mut source, _) = split_blocks(&text);
            let read;
            if source.trim().is_empty() {
                let image = crate::ocr::image_bytes(&abs, &row.format)?;
                read = generate(&x.agent, &x.url, model, &c.ocr_prompt, Some(&image), 1200)?;
                source = if read.trim().eq_ignore_ascii_case("none") { "" } else { read.trim() };
                if source.is_empty() {
                    return Err("no text found in the image; the tag stays".into());
                }
            }
            // Translation is text-only, image or not: with the image attached the same vision model softened
            // "хуй" to "crap" under the strict prompt, and from the text alone it translated the word (2026-09-22).
            let translator = c.translate_model.as_deref().unwrap_or(model);
            let prompt = if c.translate_model.is_some() {
                c.translate_prompt.replace("{text}", source)
            } else {
                format!(
                    "Translate the following text into English, line by line, keeping the same number of lines. \
                     Translate profanity and slurs literally; do not soften, censor or paraphrase. Leave any line \
                     you cannot read exactly as it is. If the text is already English, reply with it unchanged. \
                     Reply with the translation only, no commentary.\n\n{source}"
                )
            };
            let english = generate(&x.agent, &x.url, translator, &prompt, None, 1200)?;
            if english.trim().is_empty() {
                return Err("the model returned no translation; the tag stays".into());
            }
            let base = if text.trim().is_empty() { source.to_string() } else { text };
            Ok(Some(with_block(&base, ENGLISH, &english)))
        }
        "meta:reocr request" => {
            if row.kind != "image" {
                return Err(format!("{} is not an image; the tag stays", row.kind));
            }
            let model = vision_model(c)?;
            let image = crate::ocr::image_bytes(&abs, &row.format)?;
            let mut read = generate(&x.agent, &x.url, model, &c.ocr_prompt, Some(&image), 1200)?;
            if read.trim().eq_ignore_ascii_case("none") {
                read.clear();
            }
            // the machine table follows, whatever the file says; text_meta marks it done by this engine
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let tx = x.db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
            tx.execute("DELETE FROM text WHERE path=?1", params![rel]).ok();
            tx.execute("INSERT INTO text(path, body) VALUES(?1,?2)", params![rel, read])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO text_meta(path, engine, at, secs) VALUES(?1,?2,?3,0)",
                params![rel, crate::ocr::current_label(c), now],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            if manual {
                println!("  {rel}: text is reviewed; the new reading went to the machine table only");
                return Ok(None);
            }
            // the file's text is machine text: replace the reading, keep any blocks other passes added
            let (_, blocks) = split_blocks(&text);
            let new = if blocks.is_empty() {
                read
            } else if read.trim().is_empty() {
                blocks.to_string()
            } else {
                format!("{}\n\n{blocks}", read.trim_end())
            };
            Ok(Some(new))
        }
        "meta:speech request" => {
            if !has_audio(&abs)? {
                return Err("no audio track; the tag stays".into());
            }
            let wav = extract_audio(&abs)?;
            let spoken = transcribe(c, &wav);
            let _ = std::fs::remove_file(&wav);
            let spoken = spoken?;
            if spoken.trim().is_empty() {
                return Err("no speech recognised; the tag stays".into());
            }
            Ok(Some(with_block(&text, SPEECH, &spoken)))
        }
        "meta:describe request" => {
            if row.kind != "image" {
                return Err(format!("{} is not an image; the tag stays", row.kind));
            }
            let model = vision_model(c)?;
            let image = crate::ocr::image_bytes(&abs, &row.format)?;
            let prompt = "Describe what is in this picture in two or three plain sentences: who or what is shown, \
                          what they are doing, notable objects, and the setting. Name characters, people or brands \
                          if you recognise them. Do not transcribe text in the image and do not add commentary. \
                          Reply with the description only.";
            let described = generate(&x.agent, &x.url, model, prompt, Some(&image), 1200)?;
            if described.trim().is_empty() {
                return Err("the model returned no description; the tag stays".into());
            }
            Ok(Some(with_block(&text, DESCRIPTION, &described)))
        }
        other => Err(format!("no handler for {other}")),
    }
    .map(|new| {
        let _ = before;
        new
    })
}

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let o = parse(args)?;
    let db = Db::open_cfg(&c)?;
    // what is asked for, per action
    let mut work: Vec<(&Action, Vec<String>)> = vec![];
    for a in ACTIONS {
        if o.only.as_deref().is_some_and(|t| t != a.request) {
            continue;
        }
        let mut files = files_with(&db, &c.vocab.canon(a.request))?;
        if o.limit > 0 {
            files.truncate(o.limit);
        }
        if !files.is_empty() {
            work.push((a, files));
        }
    }
    if work.is_empty() {
        println!("meta: no file carries a request tag — nothing to do");
        return Ok(());
    }
    if o.dry {
        for (a, files) in &work {
            for p in files {
                println!("  {p}  ({})", a.request);
            }
        }
        println!(
            "meta: {} files would be handled (dry run)",
            work.iter().map(|(_, f)| f.len()).sum::<usize>()
        );
        return Ok(());
    }
    let needs_ollama = work.iter().any(|(a, _)| a.request != "meta:speech request");
    if needs_ollama {
        crate::ollama::reachable(&c.ollama_url)?;
    }
    let x = Ctx {
        c,
        db: &db,
        agent: crate::ollama::agent(),
        url: format!("{}/api/generate", c.ollama_url),
    };
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        let _ = ctrlc::set_handler(move || {
            s.store(true, Ordering::SeqCst);
            eprintln!("meta: stop requested — finishing the current file");
        });
    }
    let wanted: std::collections::HashSet<&String> =
        work.iter().flat_map(|(_, f)| f.iter()).collect();
    let rows: std::collections::HashMap<String, crate::index::FileRow> = db
        .all()?
        .into_iter()
        .filter(|r| wanted.contains(&r.path))
        .map(|r| (r.path.clone(), r))
        .collect();
    let (mut done, mut errs) = (0usize, 0usize);
    let t0 = Instant::now();
    'all: for (a, files) in &work {
        eprintln!("meta: {} files carry \"{}\"", files.len(), a.request);
        let (from, to) = (c.vocab.canon(a.request), c.vocab.canon(a.done));
        for rel in files {
            if stop.load(Ordering::SeqCst) {
                break 'all;
            }
            let t = Instant::now();
            let res = (|| -> Result<(), String> {
                let row = rows.get(rel).ok_or("not in the index")?;
                let abs = c.file_path(rel)?;
                let before = std::fs::read(&abs).map_err(|e| e.to_string())?;
                let packet = crate::containers::get_xmp(&before)?;
                let parsed = packet
                    .as_deref()
                    .map(crate::xmp::read)
                    .transpose()?
                    .unwrap_or_default();
                let manual = parsed.fields.get("textSource").map(String::as_str) == Some("manual");
                let new_text = perform(&x, a, rel, row, &parsed, &before)?;
                let mut fields: Vec<(&str, String)> = vec![];
                if let Some(text) = new_text {
                    fields.push(("text", text));
                    if manual {
                        fields.push(("textSource", "manual".into())); // the reviewed mark stays
                    }
                }
                let ops = [format!("-{from}"), format!("+{to}")];
                let out = crate::tagger::edit_bytes(c, &abs, before, &ops, &fields)?;
                db.upsert_cfg(c, &abs, &out)?;
                Ok(())
            })();
            match res {
                Ok(()) => {
                    done += 1;
                    println!("{} {rel} ({:.1} s)", a.done, t.elapsed().as_secs_f64());
                }
                Err(e) => {
                    errs += 1;
                    eprintln!("skip {rel}: {e}");
                }
            }
        }
    }
    println!(
        "meta: {done} done, {errs} left with their request tag, {:.1} s",
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn translation_block_sits_under_a_blank_line_and_is_replaced_on_a_rerun() {
        let once = with_block("猫は箱の中\n", ENGLISH, "The cat is in the box");
        assert_eq!(
            once,
            "猫は箱の中\n\nEnglish (machine):\nThe cat is in the box"
        );
        let twice = with_block(&once, ENGLISH, "The cat is inside the box");
        assert_eq!(
            twice,
            "猫は箱の中\n\nEnglish (machine):\nThe cat is inside the box"
        );
    }
    #[test]
    fn blocks_of_different_kinds_coexist_and_only_their_own_is_replaced() {
        let t = with_block("orig", ENGLISH, "one");
        let t = with_block(&t, DESCRIPTION, "a cat");
        assert_eq!(
            t,
            "orig\n\nEnglish (machine):\none\n\nDescription (machine):\na cat"
        );
        let t = with_block(&t, ENGLISH, "two"); // the middle block goes, the new one lands at the end
        assert_eq!(
            t,
            "orig\n\nDescription (machine):\na cat\n\nEnglish (machine):\ntwo"
        );
        assert_eq!(
            split_blocks(&t),
            (
                "orig",
                "Description (machine):\na cat\n\nEnglish (machine):\ntwo"
            )
        );
        assert_eq!(
            with_block("", SPEECH, "hello there"),
            "Speech (machine):\nhello there"
        );
        assert_eq!(split_blocks("plain\n"), ("plain", ""));
    }
    #[test]
    fn action_tags_are_canonical_namespaced_and_distinct() {
        let v = crate::vocab::Vocab::default();
        let mut seen = std::collections::HashSet::new();
        for a in ACTIONS {
            for t in [a.request, a.done] {
                assert!(t.starts_with("meta:"), "{t}");
                assert_eq!(v.canon(t), t);
                assert!(seen.insert(t), "{t} used twice");
            }
        }
        assert_eq!(ACTIONS[0].request, TRANSLATE);
        assert_eq!(ACTIONS[0].done, "meta:translated");
    }
}
