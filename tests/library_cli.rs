use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[test]
fn folders_hide_rows_preserve_caches_and_bound_indexing() {
    let home = std::env::temp_dir().join(format!("memetag-folders-cli-{}", std::process::id()));
    let root = home.join("collection");
    for dir in ["cats", "cats2", "dogs"] {
        fs::create_dir_all(root.join(dir)).unwrap();
        image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]))
            .save(root.join(dir).join("a.png"))
            .unwrap();
    }
    let command = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_memetag"));
        c.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("cfg"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CACHE_HOME", home.join("cache"));
        for key in [
            "MEMETAG_ROOT",
            "MEMETAG_DB",
            "MEMETAG_THUMBS",
            "MEMETAG_JOURNAL",
        ] {
            c.env_remove(key);
        }
        c
    };
    let run = |args: &[&str]| {
        let out = command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    run(&["init", root.to_str().unwrap()]);
    assert!(!command()
        .args(["init", root.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    run(&["reindex"]);
    let db_path = home.join("data/memetag/index.sqlite");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute("INSERT INTO text VALUES('main/cats/a.png','saved OCR')", [])
        .unwrap();
    db.execute(
        "INSERT INTO phash2 VALUES('main/cats/a.png','clip-vit-b32',?1)",
        [vec![0u8; 2048]],
    )
    .unwrap();
    run(&["folders", "exclude", "cats"]);
    assert!(!run(&["search", "path:cats/a.png"]).contains("cats/a.png"));
    assert!(run(&["search", "path:cats2/a.png"]).contains("cats2/a.png"));
    run(&["reindex"]);
    assert_eq!(
        db.query_row(
            "SELECT body FROM text WHERE path='main/cats/a.png'",
            [],
            |r| { r.get::<_, String>(0) }
        )
        .unwrap(),
        "saved OCR"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM phash2 WHERE path='main/cats/a.png'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    run(&["folders", "include", "cats"]);
    assert!(run(&["search", "text:*saved*"]).contains("cats/a.png"));
    run(&["folders", "none"]);
    assert!(run(&["search", "*"]).trim().is_empty());
    run(&["pull", "--no-thumbs", "--force"]);
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    run(&["folders", "include", "cats"]);
    run(&["pull", "--no-thumbs", "--force"]);
    assert!(run(&["search", "text:*saved*"]).contains("cats/a.png"));
    // Scoped workers prune excluded branches; old requests still list the complete collection.
    for scoped in [false, true] {
        let request = if scoped {
            serde_json::json!({"List":{"root":root,"scope":{"include":["cats"],"exclude":[]}}})
        } else {
            serde_json::json!({"List":{"root":root}})
        };
        let mut child = command()
            .arg("_pull-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let out = child.wait_with_output().unwrap();
        let reply: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let entries = reply["Ok"]["List"].as_array().unwrap();
        assert_eq!(entries.len(), if scoped { 1 } else { 3 });
    }
    assert!(!command()
        .args(["folders", "include", "../outside"])
        .output()
        .unwrap()
        .status
        .success());
    // Failed listings must preserve selected rows too.
    fs::rename(&root, home.join("offline")).unwrap();
    assert!(!command().arg("reindex").output().unwrap().status.success());
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(db);
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn help_and_versions_do_not_create_state() {
    let home = std::env::temp_dir().join(format!("memetag-versions-cli-{}", std::process::id()));
    for arg in ["--version", "--help"] {
        let out = Command::new(env!("CARGO_BIN_EXE_memetag"))
            .arg(arg)
            .env("HOME", &home)
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_CONFIG_HOME", home.join("cfg"))
            .env("XDG_CACHE_HOME", home.join("cache"))
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(!Path::new(&home).exists());
    }
}
