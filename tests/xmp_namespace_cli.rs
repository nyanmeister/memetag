use memetag::{containers, xmp::DEFAULT_MEME};
use std::{fs, path::Path, process::Command};

const OLD: &str = "https://legacy.example/ns/meme/1.0/";
const CONFIG_ONLY: &str = "https://config.example/ns/meme/1.0/";

fn run(root: &Path, args: &[&str], overrides: &[(&str, &str)]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_memetag"));
    cmd.args(args)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"));
    for key in [
        "MEMETAG_ROOT",
        "MEMETAG_DB",
        "MEMETAG_THUMBS",
        "MEMETAG_JOURNAL",
        "MEMETAG_XMP_NAMESPACE",
        "MEMETAG_LEGACY_XMP_NAMESPACES",
    ] {
        cmd.env_remove(key);
    }
    for (key, value) in overrides {
        cmd.env(key, value);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn fixture(path: &Path, namespace: &str) -> String {
    image::RgbImage::from_pixel(4, 4, image::Rgb([15, 30, 45]))
        .save(path)
        .unwrap();
    let id = memetag::writer::pixel_hash(&fs::read(path).unwrap()).unwrap();
    let packet = format!(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description rdf:about="" xmlns:meme="{namespace}" meme:id="{id}"><meme:text>preserved OCR</meme:text><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">foreign title</dc:title></rdf:Description></rdf:RDF></x:xmpmeta>"#
    );
    let bytes = containers::set_xmp(&fs::read(path).unwrap(), &packet).unwrap();
    fs::write(path, bytes).unwrap();
    id
}

#[test]
fn namespace_settings_override_and_migrate_without_losing_metadata() {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("memetag-ns-cli-{}-{stamp}", std::process::id()));
    let collection = root.join("collection");
    fs::create_dir_all(&collection).unwrap();
    run(&root, &["init", collection.to_str().unwrap()], &[]);
    let cfg_path = root.join("config/memetag/config.toml");
    let mut cfg = fs::read_to_string(&cfg_path)
        .unwrap()
        .parse::<toml::Table>()
        .unwrap();
    cfg.insert("xmp_namespace".into(), OLD.into());
    cfg.insert("legacy_xmp_namespaces".into(), vec![CONFIG_ONLY].into());
    fs::write(&cfg_path, toml::to_string(&cfg).unwrap()).unwrap();
    let old = collection.join("old.png");
    let config_only = collection.join("config.png");
    let id = fixture(&old, OLD);
    let config_id = fixture(&config_only, CONFIG_ONLY);
    assert!(run(&root, &["read", old.to_str().unwrap()], &[]).contains(&format!("meme:id = {id}")));
    assert!(run(&root, &["read", config_only.to_str().unwrap()], &[])
        .contains(&format!("meme:id = {config_id}")));

    let overrides = [
        ("MEMETAG_XMP_NAMESPACE", DEFAULT_MEME),
        ("MEMETAG_LEGACY_XMP_NAMESPACES", OLD),
    ];
    // The environment replaces the legacy list: a namespace only in the config is foreign.
    assert!(!run(&root, &["read", config_only.to_str().unwrap()], &overrides).contains("meme:id"));
    let before = run(&root, &["read", old.to_str().unwrap()], &overrides);
    assert!(before.contains("meme:text = preserved OCR"));
    run(
        &root,
        &["tag", old.to_str().unwrap(), "+migrated"],
        &overrides,
    );
    let after = run(&root, &["read", old.to_str().unwrap()], &overrides);
    assert!(after.contains(&format!("meme:id = {id}")), "{after}");
    assert!(after.contains("meme:text = preserved OCR"), "{after}");
    assert!(after.contains("migrated"), "{after}");
    let data = fs::read(&old).unwrap();
    let packet = containers::get_xmp(&data).unwrap().unwrap();
    assert!(packet.contains("foreign title"));
    assert!(
        packet.contains(&format!("<meme:id xmlns:meme=\"{DEFAULT_MEME}\">{id}")),
        "{packet}"
    );
    assert!(!packet.contains(&format!("meme:id=\"{id}\"")));
    fs::remove_dir_all(root).unwrap();
}
