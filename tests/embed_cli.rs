use std::os::unix::fs::MetadataExt;
use std::{fs, process::Command};

#[test]
fn embed_cli_dry_run_limit_resume_and_failed_retry() {
    let root = std::env::temp_dir().join(format!("memetag-cli-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let db_path = root.join("ocr.sqlite");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch("CREATE TABLE files(path TEXT, kind TEXT); CREATE TABLE text(path TEXT, body TEXT);
        INSERT INTO files VALUES('a.png','image'),('b.png','image'),('missing.png','image');
        INSERT INTO text VALUES('a.png','Already inferred café'),('b.png','Second caption'),('missing.png','Retry caption');").unwrap();
    drop(db);
    for name in ["a.png", "b.png"] {
        image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]))
            .save(root.join(name))
            .unwrap();
    }
    let source = fs::read(&db_path).unwrap();
    let original = fs::read(root.join("a.png")).unwrap();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_memetag"))
            .arg("embed-text")
            .arg("--db")
            .arg(&db_path)
            .arg("--root")
            .arg(&root)
            .args(args)
            .output()
            .unwrap()
    };
    assert!(run(&["--dry-run", "--limit", "1"]).status.success());
    assert_eq!(original, fs::read(root.join("a.png")).unwrap());
    assert!(run(&["--limit", "1"]).status.success());
    let first = fs::metadata(root.join("a.png")).unwrap();
    assert_ne!(original, fs::read(root.join("a.png")).unwrap());
    assert_eq!(original, fs::read(root.join("b.png")).unwrap());
    let retry = run(&[]);
    assert!(!retry.status.success());
    assert!(String::from_utf8_lossy(&retry.stderr).contains("missing.png"));
    assert_eq!(
        first.ctime_nsec(),
        fs::metadata(root.join("a.png")).unwrap().ctime_nsec()
    );
    fs::write(root.join("missing.png"), &original).unwrap();
    let resumed = run(&[]);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("1 embedded, 2 already match"));
    assert_eq!(source, fs::read(&db_path).unwrap());
    fs::remove_dir_all(root).unwrap();
}
