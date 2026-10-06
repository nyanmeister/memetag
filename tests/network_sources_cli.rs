//! Real CLI/workers over a fake SSH transport: no network or mounted filesystem needed.
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::{Command, Output},
};
fn command(home: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_memetag"));
    c.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("cfg"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("TEST_WORKER", env!("CARGO_BIN_EXE_memetag"))
        .env("TEST_SSH_LOG", home.join("ssh.log"));
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
fn id(added: &str) -> String {
    added
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Added source ")
        .unwrap()
        .into()
}
fn image(path: &Path) {
    image::RgbImage::from_pixel(4, 4, image::Rgb([12, 34, 56]))
        .save(path)
        .unwrap();
}
#[test]
fn network_mappings_server_writes_scopes_and_independent_failures() {
    let home = std::env::temp_dir().join(format!("memetag-network-{}", std::process::id()));
    let main = home.join("main");
    let server = home.join("server");
    let second = home.join("second-server");
    let mounted = home.join("offline-mount");
    let mounted2 = home.join("second-mount");
    for dir in [&main, &server, &second, &home.join("bin")] {
        fs::create_dir_all(dir).unwrap();
    }
    fs::create_dir_all(server.join("cats")).unwrap();
    image(&main.join("a.png"));
    image(&server.join("a.png"));
    image(&server.join("cats/a.png"));
    image(&second.join("a.png"));
    let ssh = home.join("bin/ssh");
    fs::write(
        &ssh,
        r#"#!/usr/bin/env bash
set -euo pipefail
host=${@: -2:1}
cmd=${@: -1}
printf '%s|%s\n' "$host" "$cmd" >> "$TEST_SSH_LOG"
if [[ "$host" == "${TEST_OFFLINE_HOST:-}" ]]; then echo 'fixture offline' >&2; exit 255; fi
case "$cmd" in
    *' _pull-worker') exec "$TEST_WORKER" _pull-worker ;;
    *' _batch-worker') exec "$TEST_WORKER" _batch-worker ;;
    'cat -- '*) exec bash -c "$cmd" ;;
    *) echo "Unexpected SSH command: $cmd" >&2; exit 99 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o755)).unwrap();
    run(&home, &["init", main.to_str().unwrap()]);
    let a = id(&run(
        &home,
        &[
            "sources",
            "add-network",
            "Network",
            mounted.to_str().unwrap(),
            "--host",
            "fixture",
            "--server-root",
            server.to_str().unwrap(),
        ],
    ));
    let b = id(&run(
        &home,
        &[
            "sources",
            "add-network",
            "Other",
            mounted2.to_str().unwrap(),
            "--host",
            "second",
            "--server-root",
            second.to_str().unwrap(),
        ],
    ));
    assert!(!mounted.exists());
    assert!(!mounted2.exists());
    assert!(
        !home.join("ssh.log").exists(),
        "registration must not connect or walk the mount"
    );
    // Reusing an existing server copies its helpers, but defaults to the NEW folder.
    let template_path = home.join("another-offline-mount");
    let template = id(&run(
        &home,
        &[
            "sources",
            "add-network",
            "Template",
            template_path.to_str().unwrap(),
            "--server",
            &a,
        ],
    ));
    let registry: memetag::sources::Library =
        toml::from_str(&fs::read_to_string(home.join("cfg/memetag/sources.toml")).unwrap())
            .unwrap();
    let remote = registry.source(&template).unwrap().remote.as_ref().unwrap();
    assert_eq!(remote.host, "fixture");
    assert_eq!(remote.root, template_path);
    assert_eq!(
        remote.pull_command,
        registry
            .source(&a)
            .unwrap()
            .remote
            .as_ref()
            .unwrap()
            .pull_command
    );
    run(&home, &["sources", "disable", &template]);
    for options in [
        vec!["--host", "-bad"],
        vec!["--host", "two hosts"],
        vec!["--host", "fixture", "--server-root", "/"],
        vec!["--host", "fixture", "--server-root", "/a/../b"],
        vec!["--host", "fixture", "--pull-command", ""],
        vec!["--host", "fixture", "--server-root"],
        vec!["--host", "fixture", "--host", "second"],
        vec!["--host", "fixture", "--server", "main"],
    ] {
        let mut args = vec!["sources", "add-network", "Invalid", "/offline/invalid"];
        args.extend(options);
        rejected(&home, &args);
    }
    rejected(
        &home,
        &[
            "sources",
            "add-network",
            "Overlap",
            "/offline/overlap",
            "--server",
            &a,
            "--server-root",
            server.join("cats").to_str().unwrap(),
        ],
    );
    run(&home, &["pull", "--force", "--no-thumbs"]);
    assert_eq!(run(&home, &["search", ""]).lines().count(), 4);
    assert_eq!(run(&home, &["search", "source:Network"]).lines().count(), 2);
    assert!(run(&home, &["search", "source:Other"]).contains(mounted2.to_str().unwrap()));
    // Source-scoped caches survive exclusion, disable and failed network refreshes.
    let db = rusqlite::Connection::open(home.join("data/memetag/index.sqlite")).unwrap();
    let key = format!("{a}/a.png");
    db.execute("INSERT INTO text VALUES(?1,'retained OCR')", [&key])
        .unwrap();
    db.execute(
        "INSERT INTO phash2 VALUES(?1,'clip-vit-b32',?2)",
        rusqlite::params![key, vec![7u8; 2048]],
    )
    .unwrap();
    run(&home, &["folders", "--source", &a, "exclude", "cats"]);
    run(&home, &["pull", "--force", "--no-thumbs"]);
    assert_eq!(run(&home, &["search", ""]).lines().count(), 3);
    assert_eq!(
        db.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    image(&main.join("new.png"));
    image(&second.join("new.png"));
    let out = command(&home)
        .env("TEST_OFFLINE_HOST", "fixture")
        .args(["pull", "--force", "--no-thumbs"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(run(&home, &["search", ""]).lines().count(), 5);
    assert_eq!(
        db.query_row("SELECT body FROM text WHERE path=?1", [&key], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "retained OCR"
    );
    assert_eq!(
        db.query_row(
            "SELECT length(hash) FROM phash2 WHERE path=?1",
            [&key],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2048
    );
    let relocated = home.join("relocated-offline-mount");
    run(
        &home,
        &["sources", "locate", &a, relocated.to_str().unwrap()],
    );
    assert!(!relocated.exists());
    assert!(run(&home, &["search", "source:Network"]).contains(relocated.to_str().unwrap()));
    // A mounted copy simulates SSHFS caching: writes must change the server, not this stale copy.
    fs::create_dir_all(&relocated).unwrap();
    fs::copy(server.join("a.png"), relocated.join("a.png")).unwrap();
    let before = fs::read(server.join("a.png")).unwrap();
    let md = fs::metadata(server.join("a.png")).unwrap();
    run(
        &home,
        &[
            "tag",
            relocated.join("a.png").to_str().unwrap(),
            "+network-tag",
        ],
    );
    let after = fs::read(server.join("a.png")).unwrap();
    assert_ne!(before, after);
    assert_eq!(fs::read(relocated.join("a.png")).unwrap(), before);
    let new_md = fs::metadata(server.join("a.png")).unwrap();
    assert_eq!(md.ino(), new_md.ino());
    assert_eq!(md.modified().unwrap(), new_md.modified().unwrap());
    assert_eq!(
        memetag::writer::pixel_hash(&before),
        memetag::writer::pixel_hash(&after)
    );
    assert!(
        memetag::xmp::read(&memetag::containers::get_xmp(&after).unwrap().unwrap())
            .unwrap()
            .tags
            .contains(&"network-tag".into())
    );
    rejected(
        &home,
        &[
            "tag",
            relocated.join("a.png").to_str().unwrap(),
            "+stale-tag",
        ],
    );
    // A mapped symlink outside the server root must never be writable through the helper.
    let escape = home.join("escape.png");
    image(&escape);
    std::os::unix::fs::symlink(&escape, server.join("escape.png")).unwrap();
    fs::copy(&escape, relocated.join("escape.png")).unwrap();
    let escape_before = fs::read(&escape).unwrap();
    rejected(
        &home,
        &[
            "tag",
            relocated.join("escape.png").to_str().unwrap(),
            "+escape-tag",
        ],
    );
    assert_eq!(fs::read(&escape).unwrap(), escape_before);
    run(&home, &["pull", "--force", "--no-thumbs"]);
    assert!(run(&home, &["search", "network-tag"]).contains(relocated.to_str().unwrap()));
    assert!(
        !fs::read_to_string(home.join("ssh.log"))
            .unwrap()
            .contains("vocab"),
        "additional servers must not synchronize the global vocabulary"
    );
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        3
    );
    assert!(
        db.query_row(
            "SELECT count(*) FROM files WHERE path LIKE ?1",
            [format!("{b}/%")],
            |r| r.get::<_, i64>(0)
        )
        .unwrap()
            >= 2
    );
    drop(db);
    fs::remove_dir_all(home).unwrap();
}
