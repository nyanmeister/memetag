//! A reversible view of the collection. Excluded rows remain cached, with their OCR and vectors.
use crate::{locking, Cfg};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default = "all")]
    pub include: Vec<PathBuf>,
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
}
fn all() -> Vec<PathBuf> {
    vec![PathBuf::from(".")]
}
impl Default for Scope {
    fn default() -> Self {
        Self {
            include: all(),
            exclude: vec![],
        }
    }
}
impl Scope {
    pub fn validate(&self) -> Result<(), String> {
        for p in self.include.iter().chain(&self.exclude) {
            if p.as_os_str().is_empty()
                || p.is_absolute()
                || p.components()
                    .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
            {
                return Err(format!(
                    "{}: use a folder relative to the collection root (no '..')",
                    p.display()
                ));
            }
        }
        Ok(())
    }
    fn beneath(path: &Path, dir: &Path) -> bool {
        dir == Path::new(".") || path.starts_with(dir)
    }
    pub fn contains(&self, path: &Path) -> bool {
        let depth = |p: &PathBuf| {
            p.components()
                .filter(|c| matches!(c, Component::Normal(_)))
                .count()
        };
        let included = self
            .include
            .iter()
            .filter(|p| Self::beneath(path, p))
            .map(depth)
            .max();
        let excluded = self
            .exclude
            .iter()
            .filter(|p| Self::beneath(path, p))
            .map(depth)
            .max();
        included.is_some_and(|n| excluded.is_none_or(|m| n > m))
    }
    pub fn traverse(&self, dir: &Path) -> bool {
        dir.as_os_str().is_empty()
            || self.contains(dir)
            || self
                .include
                .iter()
                .any(|p| p.starts_with(dir) && self.contains(p))
    }
    pub fn set_included(&mut self, dir: PathBuf, enabled: bool) {
        let dir: PathBuf = if dir == Path::new(".") {
            dir
        } else {
            dir.components()
                .filter(|c| *c != Component::CurDir)
                .collect()
        };
        self.include.retain(|p| p != &dir);
        self.exclude.retain(|p| p != &dir);
        if enabled {
            self.include.push(dir);
        } else {
            self.exclude.push(dir);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Saved {
    root: PathBuf,
    #[serde(flatten)]
    scope: Scope,
}
pub struct Draft {
    pub scope: Scope,
    pub root: PathBuf,
    path: PathBuf,
    original: Option<Vec<u8>>,
}
pub fn path() -> PathBuf {
    crate::paths::config_dir().join("library.toml")
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
impl Draft {
    pub fn load(root: &Path) -> Result<Self, String> {
        Self::at(path(), root)
    }
    fn at(path: PathBuf, root: &Path) -> Result<Self, String> {
        let original = read(&path)?;
        let scope = if let Some(b) = &original {
            let mut saved: Saved =
                toml::from_str(std::str::from_utf8(b).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("library configuration: {e}"))?;
            if saved.root != root {
                return Err(
                    "Collection root changed; review library.toml before indexing the new root"
                        .into(),
                );
            }
            saved.scope.validate()?;
            for path in saved
                .scope
                .include
                .iter_mut()
                .chain(&mut saved.scope.exclude)
            {
                let normalized: PathBuf = path
                    .components()
                    .filter(|c| *c != Component::CurDir)
                    .collect();
                *path = if normalized.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    normalized
                };
            }
            saved.scope
        } else {
            Scope::default()
        };
        Ok(Self {
            scope,
            root: root.into(),
            path,
            original,
        })
    }
    pub fn save(&mut self) -> Result<(), String> {
        self.scope.validate()?;
        let parent = self
            .path
            .parent()
            .ok_or("library configuration has no parent")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let _lock = locking::sidecar(&self.path.with_extension("lock"))?;
        if read(&self.path)? != self.original {
            return Err(
                "Folder selection changed in another window; reopen it before saving".into(),
            );
        }
        let bytes = toml::to_string_pretty(&Saved {
            root: self.root.clone(),
            scope: self.scope.clone(),
        })
        .map_err(|e| e.to_string())?
        .into_bytes();
        let tmp = self
            .path
            .with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| -> Result<(), String> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| e.to_string())?;
            f.write_all(&bytes)
                .and_then(|_| f.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result?;
        self.original = Some(bytes);
        Ok(())
    }
}
pub fn load(root: &Path) -> Result<Scope, String> {
    Draft::load(root).map(|d| d.scope)
}

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    if args.first().is_some_and(|a| a == "menu") {
        use std::os::unix::process::CommandExt;
        return Err(std::process::Command::new(crate::companion("memetag-gui")?)
            .arg("--folders")
            .exec()
            .to_string());
    }
    let mut draft = crate::sources::Draft::load(&c.root)?;
    let (id, args) = if args.first().is_some_and(|a| a == "--source") {
        (
            args.get(1).ok_or("--source needs an ID")?.as_str(),
            &args[2..],
        )
    } else {
        ("main", args)
    };
    let source = draft
        .library
        .sources
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or("Unknown source")?;
    let mut changed = true;
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["status"] => changed = false,
        ["all"] => source.scope = Scope::default(),
        ["none"] => {
            source.scope = Scope {
                include: vec![],
                exclude: vec![],
            }
        }
        ["include", dir] => source.scope.set_included(PathBuf::from(dir), true),
        ["exclude", dir] => source.scope.set_included(PathBuf::from(dir), false),
        _ => return Err(
            "usage: memetag folders [--source ID] [status|include DIR|exclude DIR|all|none|menu]"
                .into(),
        ),
    }
    if changed {
        draft.save()?;
    }
    let source = draft.library.source(id)?;
    println!(
        "source: {} ({})\nroot: {}\nselection: {}",
        source.name,
        source.id,
        source.path.display(),
        crate::sources::path().display()
    );
    for p in &source.scope.include {
        println!("include {}", p.display());
    }
    for p in &source.scope.exclude {
        println!("exclude {}", p.display());
    }
    if source.scope.include.is_empty() {
        println!("no folders included");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folder_boundaries_and_pruned_ancestors() {
        let s = Scope {
            include: vec!["photos/cats".into()],
            exclude: vec!["photos/cats/private".into()],
        };
        assert!(s.traverse(Path::new("photos")));
        assert!(s.contains(Path::new("photos/cats/a.png")));
        assert!(!s.contains(Path::new("photos/cats2/a.png")));
        assert!(!s.traverse(Path::new("photos/cats/private")));
        assert!(!s.traverse(Path::new("elsewhere")));
        assert!(Scope {
            include: vec!["../other".into()],
            exclude: vec![]
        }
        .validate()
        .is_err());
        assert!(!Scope {
            include: vec![],
            exclude: vec![]
        }
        .contains(Path::new("a.png")));
        let mut nested = Scope::default();
        nested.set_included("photos".into(), false);
        nested.set_included("photos/cats".into(), true);
        assert!(nested.traverse(Path::new("photos")));
        assert!(nested.contains(Path::new("photos/cats/a.png")));
        assert!(!nested.contains(Path::new("photos/dogs/a.png")));
    }
    #[test]
    fn stale_drafts_and_changed_roots_are_rejected() {
        let dir = std::env::temp_dir().join(format!("memetag-library-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.toml");
        let mut a = Draft::at(path.clone(), Path::new("/collection")).unwrap();
        let mut b = Draft::at(path.clone(), Path::new("/collection")).unwrap();
        a.scope.exclude.push("private".into());
        a.save().unwrap();
        assert!(b.save().unwrap_err().contains("another window"));
        assert!(Draft::at(path.clone(), Path::new("/other")).is_err());
        assert_eq!(
            Draft::at(path, Path::new("/collection")).unwrap().scope,
            a.scope
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
