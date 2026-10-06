//! Isolated process: one test owns its config environment for the whole test.
use memetag::{
    index::Db,
    sources::{self, Draft},
};
use std::{fs, path::Path};
#[test]
fn draft_conflicts_stale_commands_and_replaced_folders_fail_closed() {
    let home = std::env::temp_dir().join(format!("memetag-source-state-{}", std::process::id()));
    let root = home.join("images");
    let outside = home.join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::env::set_var("XDG_CONFIG_HOME", home.join("config"));
    std::env::set_var("XDG_DATA_HOME", home.join("data"));
    for name in ["MEMETAG_ROOT", "MEMETAG_DB"] {
        std::env::remove_var(name);
    }
    memetag::setup::init(root.to_str().unwrap()).unwrap();
    let c = memetag::cfg();
    Db::open_cfg(&c).unwrap();
    let mut a = Draft::load(&root).unwrap();
    let mut b = Draft::load(&root).unwrap();
    let id = a.add("Outside", &outside).unwrap();
    a.save().unwrap();
    b.library.sources[0].name = "Lost update".into();
    assert!(b.save().is_err());
    assert!(c.ensure_current().is_err());
    assert!(Db::open_cfg(&c).is_err());
    let current = memetag::cfg();
    assert!(current.ensure_current().is_ok());
    let mut expr = memetag::query::parse("source:Outside").unwrap();
    sources::resolve_query(&current, &mut expr).unwrap();
    let mut invalid = memetag::query::parse("source:Missing").unwrap();
    assert!(sources::resolve_query(&current, &mut invalid).is_err());
    assert!(current.file_path(&format!("{id}/../escape.png")).is_err());
    assert!(current.file_path("unknown/a.png").is_err());
    std::os::unix::fs::symlink(&outside, home.join("alias")).unwrap();
    assert!(a.add("Alias", &home.join("alias")).is_err());
    fs::rename(&outside, home.join("moved")).unwrap();
    fs::create_dir(&outside).unwrap();
    assert!(current.for_file(&outside.join("file.png")).is_err());
    assert!(!current
        .library()
        .unwrap()
        .unwrap()
        .source(&id)
        .unwrap()
        .available());
    a.locate(&id, &home.join("moved")).unwrap();
    a.save().unwrap();
    let new = memetag::cfg();
    assert!(new.for_file(&home.join("moved/file.png")).is_ok());
    assert!(current.ensure_current().is_err());
    // Unknown schemas must not be reset or silently recreated.
    let db = rusqlite::Connection::open(&new.db).unwrap();
    db.execute_batch("PRAGMA user_version=99").unwrap();
    assert!(Db::open_cfg(&new).is_err());
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        99
    );
    assert!(Path::new(&sources::path()).exists());
    drop(db);
    fs::remove_dir_all(home).unwrap();
}
