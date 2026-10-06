//! HTTP originals use the configured server despite an unavailable mount and
//! reject truncated SSH reads. Everything runs against disposable local files.
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    home: PathBuf,
    child: Option<Child>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}
fn command(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memetag"));
    for (key, _) in std::env::vars().filter(|(key, _)| key.starts_with("MEMETAG_")) {
        cmd.env_remove(key);
    }
    cmd.env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("FIXTURE_INTERRUPT", home.join("interrupt"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                home.join("bin").display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    cmd
}
fn cli(home: &Path, args: &[&str]) -> String {
    let out = command(home).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
fn get(port: u16, target: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut bytes = vec![];
    stream.read_to_end(&mut bytes).unwrap();
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (
        String::from_utf8(bytes[..split].to_vec()).unwrap(),
        bytes[split + 4..].to_vec(),
    )
}

#[test]
fn originals_and_copy_bypass_offline_mount_and_never_send_partial_files() {
    let home = std::env::temp_dir().join(format!("memetag-serve-{}", std::process::id()));
    let mut fixture = Fixture {
        home: home.clone(),
        child: None,
    };
    let main = home.join("main");
    let server = home.join("server");
    let mounted = home.join("unmounted");
    for p in [&main, &server, &home.join("bin")] {
        fs::create_dir_all(p).unwrap();
    }
    let name = "caption's café.png";
    image::RgbImage::from_pixel(4, 4, image::Rgb([10, 20, 30]))
        .save(server.join(name))
        .unwrap();
    let original = fs::read(server.join(name)).unwrap();
    let ssh = home.join("bin/ssh");
    fs::write(&ssh, "#!/bin/sh\nfor arg do command=$arg; done\ncase $command in\ncat*) if [ -e \"$FIXTURE_INTERRUPT\" ]; then printf incomplete-file; printf disconnected >&2; exit 255; fi;;\nesac\nexec sh -c \"$command\"\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_memetag"), home.join("bin/memetag")).unwrap();
    cli(&home, &["init", main.to_str().unwrap()]);
    let added = cli(
        &home,
        &[
            "sources",
            "add-network",
            "Remote",
            mounted.to_str().unwrap(),
            "--host",
            "fixture",
            "--server-root",
            server.to_str().unwrap(),
        ],
    );
    let id = added
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Added source ")
        .unwrap();
    cli(&home, &["pull", "--no-thumbs"]);
    assert!(!mounted.exists());
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    fixture.child = Some(
        command(&home)
            .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "server failed to start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let key = format!("{id}/caption%27s%20caf%C3%A9.png");
    for route in ["file", "png"] {
        let (header, body) = get(port, &format!("/{route}?f={key}"));
        assert!(header.starts_with("HTTP/1.1 200"), "{header}");
        assert_eq!(body, original);
    }
    fs::write(home.join("interrupt"), b"").unwrap();
    let (header, body) = get(port, &format!("/file?f={key}"));
    assert!(header.starts_with("HTTP/1.1 503"), "{header}");
    assert!(header.contains("Retry-After: 2"));
    assert!(!body
        .windows(b"incomplete-file".len())
        .any(|w| w == b"incomplete-file"));
    fs::remove_file(home.join("interrupt")).unwrap();
    let (header, body) = get(port, &format!("/file?f={key}"));
    assert!(header.starts_with("HTTP/1.1 200"));
    assert_eq!(body, original);
    let (header, _) = get(port, "/file?f=../outside.png");
    assert!(header.starts_with("HTTP/1.1 404"));

    // Older phone installations may have only pull_remote. Viewing must not
    // require a writer configuration or fall back to a disconnected mount.
    let mut child = fixture.child.take().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let config_path = home.join("config/memetag/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[pull_remote]\nlocal_root = {:?}\nroot = {:?}\nhost = \"fixture\"\ncommand = \"memetag _pull-worker\"\n",
        main.to_str().unwrap(), server.to_str().unwrap()
    ));
    fs::write(config_path, config).unwrap();
    cli(&home, &["pull", "--no-thumbs"]);
    assert!(!main.join(name).exists());
    fixture.child = Some(
        command(&home)
            .args(["serve", "--bind", &format!("127.0.0.1:{port}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let start = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
    let (header, body) = get(port, "/file?f=main/caption%27s%20caf%C3%A9.png");
    assert!(header.starts_with("HTTP/1.1 200"), "{header}");
    assert_eq!(body, original);
}
