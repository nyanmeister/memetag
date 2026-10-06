//! `memetag _fuzz <dir> <seconds> [seed]` — mutation fuzzing of every parser on copies of real files.
//! Records panics and inputs taking >1 s after they return. Use an external timeout for hangs.
use crate::{containers, query, xmp};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

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
            (self.next() % n as u64) as usize
        }
    }
}

fn mutate(rng: &mut Rng, src: &[u8]) -> Vec<u8> {
    let mut b = src.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(8) {
            0 | 1 => {
                if !b.is_empty() {
                    let i = rng.below(b.len());
                    b[i] ^= 1 << rng.below(8);
                }
            } // bit flip
            2 => {
                if !b.is_empty() {
                    let i = rng.below(b.len());
                    b[i] = rng.next() as u8;
                }
            } // random byte
            3 => {
                if b.len() > 1 {
                    let n = rng.below(b.len());
                    b.truncate(n);
                }
            } // truncate
            4 => {
                let i = rng.below(b.len() + 1);
                let n = 1 + rng.below(16);
                for _ in 0..n {
                    b.insert(i, rng.next() as u8);
                }
            } // insert junk
            5 => {
                if b.len() > 8 {
                    let i = rng.below(b.len() - 4);
                    let v = [0u8, 0xFF, 0x7F, 0x80][rng.below(4)];
                    for k in 0..4 {
                        b[i + k] = v;
                    }
                }
            } // size-field bombs
            6 => {
                if b.len() > 16 {
                    let i = rng.below(b.len() - 8);
                    let j = rng.below(b.len() - 8);
                    let chunk: Vec<u8> = b[j..j + 8].to_vec();
                    b.splice(i..i + 8, chunk);
                }
            } // block swap
            _ => {
                if b.len() > 2 {
                    let i = rng.below(b.len());
                    let n = rng.below((b.len() - i).min(64));
                    b.drain(i..i + n);
                }
            } // delete
        }
    }
    b
}

fn random_string(rng: &mut Rng) -> String {
    const ALPHA: &[u8] = b"ab :,|()-!*?.<>&\"'/\\\n\t=;0123456789_\x01";
    let n = rng.below(40);
    (0..n)
        .map(|_| {
            let c = ALPHA[rng.below(ALPHA.len())];
            if rng.below(20) == 0 {
                '\u{e9}'
            } else {
                c as char
            }
        })
        .collect()
}

fn run_guarded<F: FnOnce() + std::panic::UnwindSafe>(
    label: &str,
    input: &[u8],
    out_dir: &Path,
    seed: u64,
    iter: u64,
    f: F,
) -> (bool, bool) {
    let t0 = Instant::now();
    let panicked = std::panic::catch_unwind(f).is_err();
    let slow = t0.elapsed() > Duration::from_secs(1);
    if panicked || slow {
        let name = format!(
            "{label}-seed{seed}-iter{iter}-{}.bin",
            if panicked { "panic" } else { "slow" }
        );
        let _ = std::fs::write(out_dir.join(&name), input);
        eprintln!(
            "!! {} in {label} (seed {seed}, iter {iter}, {} bytes) → saved {name}",
            if panicked { "PANIC" } else { "SLOW" },
            input.len()
        );
    }
    (panicked, slow)
}

/// Replay one saved crasher with the normal panic hook so the location prints.
pub fn replay(file: &Path) -> Result<(), String> {
    let input = std::fs::read(file).map_err(|e| e.to_string())?;
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    if name.starts_with("query-") {
        check_query(&String::from_utf8_lossy(&input));
        return Ok(());
    }
    if name.starts_with("xmp-") {
        check_xmp(&String::from_utf8_lossy(&input));
        return Ok(());
    }
    let mut f = BTreeMap::new();
    f.insert("id".to_string(), "seed".to_string());
    let xmp_seed = xmp::merge(None, &["a".into(), "b:c".into()], &f)?;
    if name.starts_with("writer-") {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let scratch =
            std::env::temp_dir().join(format!("memetag-replay-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&scratch).map_err(|e| e.to_string())?;
        // Replay is a dedicated CLI process; isolate its recovery journal too.
        std::env::set_var("MEMETAG_JOURNAL", scratch.join("journal"));
        check_writer(&input, &xmp_seed, &scratch.join("fixture"));
        std::fs::remove_dir_all(&scratch).map_err(|e| e.to_string())?;
        return Ok(());
    }
    println!(
        "kind {} · {} bytes",
        containers::sniff(&input).label(),
        input.len()
    );
    println!(
        "get_xmp → {:?}",
        containers::get_xmp(&input).map(|o| o.map(|s| s.len()))
    );
    let set = containers::set_xmp(&input, &xmp_seed);
    println!("set_xmp → {:?}", set.as_ref().map(|v| v.len()));
    println!(
        "strip_xmp → {:?}",
        containers::strip_xmp(&input).map(|v| v.len())
    );
    if let Ok(v) = &set {
        println!(
            "get_xmp(set) → {:?}",
            containers::get_xmp(v).map(|o| o.map(|s| s.len()))
        );
        println!(
            "set_xmp(set, \"\") → {:?}",
            containers::set_xmp(v, "").map(|v| v.len())
        );
    }
    Ok(())
}

pub fn run(dir: &Path, seconds: u64, seed: u64) -> Result<(), String> {
    if std::env::var("MEMETAG_FUZZ_VERBOSE").is_err() {
        std::panic::set_hook(Box::new(|_| {}));
    } // quiet: we count panics ourselves
    let out_dir = dir.join("_fuzz_crashers");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let files: Vec<(String, Vec<u8>)> = walkdir::WalkDir::new(dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file() && !e.path().starts_with(&out_dir))
        .filter(|e| e.metadata().is_ok_and(|m| m.len() < 4_000_000))
        .filter_map(|e| {
            std::fs::read(e.path())
                .ok()
                .map(|b| (e.path().display().to_string(), b))
        })
        .filter(|(_, b)| b.len() < 4_000_000)
        .collect();
    if files.is_empty() {
        return Err("no files to fuzz".into());
    }
    let xmp_seed = {
        let mut f = BTreeMap::new();
        f.insert("id".to_string(), "seed".to_string());
        xmp::merge(None, &["a".into(), "b:c".into()], &f)?
    };
    let mut rng = Rng(seed | 1);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut iters, mut panics, mut slows, mut errs, mut oks, mut writes) =
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut per_kind: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    while Instant::now() < deadline {
        iters += 1;
        let (_, orig) = &files[rng.below(files.len())];
        let kind = containers::sniff(orig).label().to_string();
        let input = mutate(&mut rng, orig);
        // 1. container parsers: get / set / strip; then a second set on the result (idempotence path)
        let parsed_ok = std::cell::Cell::new(false);
        let (p, s) = run_guarded(
            "container",
            &input,
            &out_dir,
            seed,
            iters,
            std::panic::AssertUnwindSafe(|| {
                let got = containers::get_xmp(&input);
                parsed_ok.set(got.is_ok());
                let set = containers::set_xmp(&input, &xmp_seed);
                let _ = containers::strip_xmp(&input);
                if let Ok(v) = &set {
                    let _ = containers::get_xmp(v);
                    let _ = containers::set_xmp(v, "");
                }
            }),
        );
        panics += p as u64;
        slows += s as u64;
        let e = per_kind.entry(kind).or_default();
        if !p && parsed_ok.get() {
            e.0 += 1;
            oks += 1;
        } else {
            e.1 += 1;
            errs += 1;
        }
        // 1b. write path, every ~40th input: the file on disk must end up EITHER the new bytes (OK) OR the original bytes (rolled back) — never anything else
        if rng.below(40) == 0 {
            let wrote = std::cell::Cell::new(false);
            let (p, s) = run_guarded(
                "writer",
                &input,
                &out_dir,
                seed,
                iters,
                std::panic::AssertUnwindSafe(|| {
                    wrote.set(check_writer(
                        &input,
                        &xmp_seed,
                        &out_dir.join("_write_under_test.bin"),
                    ));
                }),
            );
            panics += p as u64;
            slows += s as u64;
            writes += wrote.get() as u64;
        }
        // 2. Always exercise XMP, including with an untagged seed corpus.
        {
            let mutated =
                String::from_utf8_lossy(&mutate(&mut rng, xmp_seed.as_bytes())).into_owned();
            let (p, s) = run_guarded("xmp", mutated.as_bytes(), &out_dir, seed, iters, || {
                check_xmp(&mutated);
            });
            panics += p as u64;
            slows += s as u64;
        }
        // 3. query grammar + glob
        let q = random_string(&mut rng);
        let (p, s) = run_guarded("query", q.as_bytes(), &out_dir, seed, iters, || {
            check_query(&q);
        });
        panics += p as u64;
        slows += s as u64;
    }
    println!("fuzz: {iters} iterations in {seconds}s, seed {seed}: {panics} panics, {slows} slow (>1 s); container parse ok/err = {oks}/{errs}; in-place writes checked: {writes}");
    for (k, (ok, err)) in per_kind {
        println!("  {k:6} ok {ok:6}  err {err:6}");
    }
    if panics + slows > 0 {
        Err(format!(
            "{} findings saved in {}",
            panics + slows,
            out_dir.display()
        ))
    } else {
        Ok(())
    }
}

fn check_writer(input: &[u8], packet: &str, path: &Path) -> bool {
    let Ok(new_bytes) = containers::set_xmp(input, packet) else {
        return false;
    };
    std::fs::write(path, input).expect("create writer fuzz fixture");
    let result = crate::writer::write_in_place(path, input, &new_bytes);
    let after = std::fs::read(path).expect("read writer fuzz fixture");
    assert!(
        after == new_bytes || after == input,
        "write left a third state: {:?}",
        result.map(|x| x.report.pixels)
    );
    std::fs::remove_file(path).expect("remove writer fuzz fixture");
    true
}

fn check_xmp(input: &str) {
    let _ = xmp::read(input);
    let fields = BTreeMap::from([("id".into(), "z".into())]);
    if let Ok(merged) = xmp::merge(Some(input), &["t".into()], &fields) {
        let _ = xmp::read(&merged);
    }
}

fn check_query(input: &str) {
    let tag = input.trim();
    if !tag.is_empty() {
        assert_eq!(
            query::parse(&query::quote_tag(tag)).unwrap(),
            query::Expr::ExactTag(tag.to_ascii_lowercase())
        );
    }
    for caret in 0..=input.chars().count() {
        if let Some((range, _)) = query::completion(input, caret) {
            assert!(range.start <= range.end && range.end <= input.len());
            assert!(input.is_char_boundary(range.start) && input.is_char_boundary(range.end));
        }
    }
    if let Ok(expr) = query::parse(input) {
        let row = crate::index::FileRow {
            id: "x".into(),
            path: "fixture/café.png".into(),
            format: "png".into(),
            kind: "image".into(),
            width: 1,
            height: 1,
            size: 1,
            mtime: 0.0,
            created_at: 0.0,
            tagged_at: 0.0,
            xmp_tag_count: 2,
            tags: ["a".into(), "b:c".into()].into(),
            text: "some caption".into(),
        };
        let item = query::Item::of(&row, &|_, _| None);
        let _ = query::eval(&expr, &item, &str::to_string);
    }
    let _ = query::glob(input, "fixture/café.png");
}
