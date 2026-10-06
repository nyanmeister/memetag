//! End-to-end smoke campaign. All writes and recovery journals stay in a scratch directory.
use std::{fs, process::Command};

#[test]
fn mutation_campaign_and_parser_replay_use_isolated_fixtures() {
    let root = std::env::temp_dir().join(format!("memetag-fuzz-cli-{}", std::process::id()));
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    fs::write(
        corpus.join("animated.png"),
        include_bytes!("fixtures/animated.png"),
    )
    .unwrap();
    fs::write(
        corpus.join("animated.webp"),
        include_bytes!("fixtures/animated.webp"),
    )
    .unwrap();
    for ext in ["png", "jpg", "gif", "webp", "bmp"] {
        image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8 * 20, y as u8 * 20, 80]))
            .save(corpus.join(format!("still.{ext}")))
            .unwrap();
    }
    for brand in [b"avif", b"isom"] {
        let mut bytes = 16u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0; 4]);
        fs::write(corpus.join(String::from_utf8_lossy(brand).as_ref()), bytes).unwrap();
    }
    let run = |args: &[&std::ffi::OsStr]| {
        Command::new(env!("CARGO_BIN_EXE_memetag"))
            .env("HOME", &root)
            .env("MEMETAG_JOURNAL", root.join("journal"))
            .args(args)
            .output()
            .unwrap()
    };
    // Permit a longer local campaign without slowing the default test suite.
    let seconds = std::env::var("MEMETAG_TEST_FUZZ_SECONDS").unwrap_or_else(|_| "1".into());
    let result = run(&[
        "_fuzz".as_ref(),
        corpus.as_os_str(),
        seconds.as_ref(),
        "12345".as_ref(),
    ]);
    assert!(
        result.status.success(),
        "artifacts retained at {}\n{}\n{}",
        root.display(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    println!("{}", String::from_utf8_lossy(&result.stdout));
    for (name, bytes) in [
        ("query-seed1-iter1-panic.bin", "猫 OR (a, -b)"),
        ("xmp-seed1-iter1-panic.bin", "<broken RDF"),
        ("writer-seed1-iter1-panic.bin", "opaque fallback payload"),
    ] {
        let path = root.join(name);
        fs::write(&path, bytes).unwrap();
        let replay = run(&["_replay".as_ref(), path.as_os_str()]);
        assert!(
            replay.status.success(),
            "{}",
            String::from_utf8_lossy(&replay.stderr)
        );
        assert!(
            !String::from_utf8_lossy(&replay.stdout).contains("get_xmp"),
            "wrong replay parser"
        );
    }
    fs::remove_dir_all(root).unwrap();
}
