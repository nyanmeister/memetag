// Exercise the production CLI with isolated config/data and real SQLite migrations.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
fn command(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memetag"));
    c.env("HOME", home)
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
}
fn run(home: &Path, args: &[&str]) -> String {
    let out = command(home).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn rejected(home: &Path, args: &[&str]) -> Output {
    let out = command(home).args(args).output().unwrap();
    assert!(!out.status.success(), "unexpected success {args:?}");
    out
}
fn image(path: &Path) {
    image::RgbImage::from_pixel(4, 4, image::Rgb([12, 34, 56]))
        .save(path)
        .unwrap();
}
fn fixture(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("memetag-sources-{name}-{}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}
#[test]
fn same_names_selection_relocation_offline_and_local_writes() {
    let home = fixture("locations");
    let main = home.join("main");
    let outside = home.join("outside");
    fs::create_dir_all(&main).unwrap();
    fs::create_dir_all(outside.join("cats")).unwrap();
    image(&main.join("a.png"));
    image(&outside.join("a.png"));
    image(&outside.join("cats/a.png"));
    run(&home, &["init", main.to_str().unwrap()]);
    let added = run(
        &home,
        &["sources", "add", "Downloads", outside.to_str().unwrap()],
    );
    let id = added
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Added source ")
        .unwrap()
        .to_owned();
    rejected(
        &home,
        &[
            "sources",
            "add",
            "Nested",
            outside.join("cats").to_str().unwrap(),
        ],
    );
    run(&home, &["reindex"]);
    assert!(!command(&home)
        .env("MEMETAG_ROOT", &outside)
        .args(["search", ""])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(run(&home, &["search", ""]).lines().count(), 3);
    assert_eq!(
        run(&home, &["search", "source:Downloads"]).lines().count(),
        2
    );
    let conn = rusqlite::Connection::open(home.join("data/memetag/index.sqlite")).unwrap();
    let key = format!("{id}/a.png");
    conn.execute("INSERT INTO text VALUES(?1,'saved OCR')", [&key])
        .unwrap();
    conn.execute(
        "INSERT INTO phash2 VALUES(?1,'clip-vit-b32',?2)",
        rusqlite::params![key, vec![7u8; 2048]],
    )
    .unwrap();
    run(
        &home,
        &[
            "tag",
            outside.join("a.png").to_str().unwrap(),
            "+outside-tag",
        ],
    );
    assert_eq!(run(&home, &["search", "outside-tag"]).lines().count(), 1);
    run(&home, &["folders", "--source", &id, "exclude", "cats"]);
    run(&home, &["reindex"]);
    assert_eq!(run(&home, &["search", ""]).lines().count(), 2);
    run(&home, &["sources", "disable", &id]);
    assert_eq!(run(&home, &["search", ""]).lines().count(), 1);
    run(&home, &["pull", "--force", "--no-thumbs"]);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    run(&home, &["sources", "enable", &id]);
    run(&home, &["sources", "rename", &id, "Local"]);
    assert_eq!(run(&home, &["search", "source:Local"]).lines().count(), 1);
    let moved = home.join("relocated");
    fs::rename(&outside, &moved).unwrap();
    rejected(&home, &["pull", "--force", "--no-thumbs"]);
    assert!(run(&home, &["sources", "status"]).contains("offline"));
    assert_eq!(
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    run(&home, &["sources", "locate", &id, moved.to_str().unwrap()]);
    run(&home, &["rescan", &key]);
    assert_eq!(
        conn.query_row("SELECT body FROM text WHERE path=?1", [&key], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "saved OCR"
    );
    assert_eq!(
        conn.query_row(
            "SELECT length(hash) FROM phash2 WHERE path=?1",
            [&key],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2048
    );
    assert!(run(&home, &["search", "text:*saved*"]).contains(moved.to_str().unwrap()));
    assert!(run(&home, &["embed-text", "--dry-run", "--source", &id]).contains(&key));
    let snapshot = fs::read(home.join("data/memetag/index.sqlite")).unwrap();
    run(
        &home,
        &[
            "embed-text",
            "--source",
            &id,
            "--root",
            moved.to_str().unwrap(),
        ],
    );
    assert_eq!(
        snapshot,
        fs::read(home.join("data/memetag/index.sqlite")).unwrap()
    );
    assert!(run(&home, &["read", moved.join("a.png").to_str().unwrap()]).contains("saved OCR"));
    // A replacement directory at the old location must not be treated as the same source.
    let moved2 = home.join("relocated-again");
    fs::rename(&moved, &moved2).unwrap();
    fs::create_dir(&moved).unwrap();
    rejected(&home, &["reindex"]);
    assert_eq!(
        conn.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    drop(conn);
    fs::remove_dir_all(home).unwrap();
}
#[test]
fn v2_migration_preserves_every_cache_and_handles_prefix_collisions() {
    let home = fixture("migration");
    let root = home.join("collection");
    fs::create_dir_all(root.join("main")).unwrap();
    image(&root.join("a.png"));
    image(&root.join("main/a.png"));
    run(&home, &["init", root.to_str().unwrap()]);
    let data = home.join("data/memetag");
    fs::create_dir_all(&data).unwrap();
    let db = rusqlite::Connection::open(data.join("index.sqlite")).unwrap();
    // Frozen pre-sources v2 schema fixture, including FTS and history.
    db.execute_batch("PRAGMA user_version=2;
    CREATE TABLE files(path TEXT PRIMARY KEY,id TEXT NOT NULL,format TEXT,kind TEXT,width INTEGER,height INTEGER,size INTEGER,mtime REAL,created_at REAL,tagged_at REAL,xmp_tag_count INTEGER,indexed_at REAL,text_in_file INTEGER);
    CREATE TABLE tags(path TEXT,tag TEXT,source TEXT,PRIMARY KEY(path,tag,source));
    CREATE VIRTUAL TABLE text USING fts5(path UNINDEXED,body);
    CREATE TABLE phash2(path TEXT,alg TEXT,hash BLOB,PRIMARY KEY(path,alg));
    CREATE TABLE text_meta(path TEXT PRIMARY KEY,engine TEXT,at REAL,secs REAL);
    CREATE TABLE manual_text(path TEXT PRIMARY KEY,body TEXT);
    CREATE TABLE proposal_rejects(tag TEXT,path TEXT,PRIMARY KEY(tag,path));
    CREATE TABLE tag_history(tag TEXT PRIMARY KEY,uses INTEGER);
    CREATE TABLE proposal_modes(tag TEXT PRIMARY KEY,mode TEXT);
    INSERT INTO files VALUES('a.png','id','png','image',4,4,1,0,0,0,1,0,0),('main/a.png','id2','png','image',4,4,1,0,0,0,1,0,0);
    INSERT INTO tags VALUES('a.png','fixture','xmp');
    INSERT INTO text VALUES('a.png','machine OCR');
    INSERT INTO phash2 VALUES('a.png','clip-vit-b32',X'010203');
    INSERT INTO text_meta VALUES('a.png','engine',123,0.5);
    INSERT INTO manual_text VALUES('main/a.png','reviewed');
    INSERT INTO proposal_rejects VALUES('rejected','a.png');
    INSERT INTO tag_history VALUES('fixture',17);
    INSERT INTO proposal_modes VALUES('fixture','both');").unwrap();
    assert_eq!(run(&home, &["search", ""]).lines().count(), 2);
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM files WHERE path IN ('main/a.png','main/main/a.png')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    for table in ["tags", "text", "phash2", "text_meta", "proposal_rejects"] {
        assert_eq!(
            db.query_row(
                &format!("SELECT count(*) FROM {table} WHERE path='main/a.png'"),
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
    assert_eq!(
        db.query_row(
            "SELECT body FROM manual_text WHERE path='main/main/a.png'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "reviewed"
    );
    assert_eq!(
        db.query_row("SELECT uses FROM tag_history", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        17
    );
    assert_eq!(
        db.query_row("SELECT mode FROM proposal_modes", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "both"
    );
    let backups: Vec<_> = fs::read_dir(&data)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("before-sources"))
        .collect();
    assert_eq!(backups.len(), 1);
    let backup = rusqlite::Connection::open(backups[0].path()).unwrap();
    assert_eq!(
        backup
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        backup
            .query_row("SELECT count(*) FROM files WHERE path='a.png'", [], |r| r
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
    assert_eq!(run(&home, &["search", "text:*machine*"]).lines().count(), 1);
    // Reopening must not migrate a second time.
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    drop(backup);
    drop(db);
    fs::remove_dir_all(home).unwrap();
}
