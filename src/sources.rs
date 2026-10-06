//! Stable source identities, independent of labels and mount points.
use crate::{index, library::Scope, locking, Cfg};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Remote {
    pub host: String,
    pub root: PathBuf,
    pub pull_command: String,
    pub batch_command: String,
}
impl Remote {
    pub fn defaults(host: String, root: PathBuf) -> Self {
        Self {
            host,
            root,
            pull_command: "nice -n 19 ionice -c 3 memetag _pull-worker".into(),
            batch_command: "nice -n 19 ionice -c 3 memetag _batch-worker".into(),
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty()
            || self.host.starts_with('-')
            || self.host.chars().any(|c| c.is_whitespace() || c == '\0')
            || !self.root.is_absolute()
            || self.root == Path::new("/")
            || self
                .root
                .components()
                .any(|c| matches!(c, Component::ParentDir))
            || self.root.as_os_str().as_encoded_bytes().contains(&0)
            || self.pull_command.trim().is_empty()
            || self.batch_command.trim().is_empty()
            || self.pull_command.contains(['\0', '\n', '\r'])
            || self.batch_command.contains(['\0', '\n', '\r'])
        {
            return Err("Use an SSH host, an absolute server folder (not / or ..), and nonempty helper commands".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub scope: Scope,
    pub remote: Option<Remote>,
    #[serde(default)]
    pub anchor: Option<String>,
}
fn yes() -> bool {
    true
}
pub fn anchor(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path)
        .ok()
        .filter(|m| m.is_dir())
        .map(|m| format!("{}:{}", m.dev(), m.ino()))
}
impl Source {
    pub fn available(&self) -> bool {
        self.remote.is_some()
            || anchor(&self.path)
                .is_some_and(|a| self.anchor.as_ref().is_none_or(|expected| *expected == a))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Library {
    pub sources: Vec<Source>,
}
pub fn path() -> PathBuf {
    crate::paths::config_dir().join("sources.toml")
}
fn read(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
pub fn relative(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty()
        || !path.components().all(|c| matches!(c, Component::Normal(_)))
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err("Invalid library file path".into());
    }
    Ok(())
}
impl Library {
    pub fn load(root: &Path) -> Result<Self, String> {
        Draft::load(root).map(|d| d.library)
    }
    fn bootstrap(root: &Path) -> Result<Self, String> {
        let mut remote = None;
        if let Some(bytes) = read(&crate::paths::config_dir().join("config.toml"))? {
            let config: toml::Table =
                toml::from_str(std::str::from_utf8(&bytes).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("configuration: {e}"))?;
            if let (Some(pull), Some(batch)) =
                (config.get("pull_remote"), config.get("batch_remote"))
            {
                let get = |table: &toml::Value, key: &str| -> Result<String, String> {
                    table
                        .get(key)
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                        .ok_or_else(|| format!("remote configuration missing {key}"))
                };
                if Path::new(&get(pull, "local_root")?) == root
                    && Path::new(&get(batch, "local_root")?) == root
                {
                    if get(pull, "host")? != get(batch, "host")?
                        || get(pull, "root")? != get(batch, "root")?
                    {
                        return Err("Pull and batch servers must agree for a source".into());
                    }
                    remote = Some(Remote {
                        host: get(pull, "host")?,
                        root: get(pull, "root")?.into(),
                        pull_command: get(pull, "command")?,
                        batch_command: get(batch, "command")?,
                    });
                }
            }
        }
        let anchor = if remote.is_none() { anchor(root) } else { None };
        Ok(Self {
            sources: vec![Source {
                id: "main".into(),
                name: "Memes".into(),
                path: root.into(),
                enabled: true,
                scope: crate::library::load(root)?,
                remote,
                anchor,
            }],
        })
    }
    pub fn validate(&self) -> Result<(), String> {
        let mut ids = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        if !self.sources.iter().any(|s| s.id == "main") {
            return Err("Library must retain its original main source".into());
        }
        for source in &self.sources {
            if source.id.is_empty()
                || !source
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                || !ids.insert(&source.id)
                || source.name.trim().is_empty()
                || !source.path.is_absolute()
                || source.path == Path::new("/")
                || source.path.as_os_str().as_encoded_bytes().contains(&0)
                || source
                    .path
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
            {
                return Err(format!("Invalid library source {}", source.name));
            }
            source.scope.validate()?;
            if !names.insert(source.name.trim().to_lowercase()) {
                return Err("Source names must be distinct".into());
            }
            if let Some(remote) = &source.remote {
                remote.validate()?;
            }
        }
        for (i, a) in self.sources.iter().enumerate() {
            for b in self.sources.iter().skip(i + 1) {
                if let (Some(ar), Some(br)) = (&a.remote, &b.remote) {
                    if ar.host == br.host
                        && (ar.root.starts_with(&br.root) || br.root.starts_with(&ar.root))
                    {
                        return Err(format!(
                            "Sources {} and {} overlap on their server",
                            a.name, b.name
                        ));
                    }
                }
                if a.path.starts_with(&b.path)
                    || b.path.starts_with(&a.path)
                    || (a.remote.is_none()
                        && b.remote.is_none()
                        && a.anchor.is_some()
                        && a.anchor == b.anchor)
                {
                    return Err(format!(
                        "Sources {} and {} overlap; select subfolders within one source instead",
                        a.name, b.name
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn source(&self, id: &str) -> Result<&Source, String> {
        self.sources
            .iter()
            .find(|s| s.id == id)
            .ok_or_else(|| format!("Unknown source {id}"))
    }
    pub fn identify<'a, 'b>(&'a self, key: &'b str) -> Result<(&'a Source, &'b Path), String> {
        let (id, rel) = key
            .split_once('/')
            .ok_or("A library file needs a source ID and relative path")?;
        let rel = Path::new(rel);
        relative(rel)?;
        Ok((self.source(id)?, rel))
    }
    pub fn key(&self, absolute: &Path) -> Result<String, String> {
        let source = self
            .sources
            .iter()
            .find(|s| absolute.starts_with(&s.path))
            .ok_or("File is outside registered sources")?;
        let rel = absolute
            .strip_prefix(&source.path)
            .map_err(|e| e.to_string())?;
        relative(rel)?;
        Ok(format!(
            "{}/{}",
            source.id,
            rel.to_str().ok_or("Library file path is not UTF-8")?
        ))
    }
    pub fn selection(&self, active: Option<&str>) -> Scope {
        let mut scope = Scope {
            include: vec![],
            exclude: vec![],
        };
        for s in &self.sources {
            if !s.enabled || active.is_some_and(|id| s.id != id) {
                continue;
            }
            for p in &s.scope.include {
                scope.include.push(
                    PathBuf::from(&s.id).join(
                        p.components()
                            .filter(|c| *c != Component::CurDir)
                            .collect::<PathBuf>(),
                    ),
                );
            }
            for p in &s.scope.exclude {
                scope.exclude.push(
                    PathBuf::from(&s.id).join(
                        p.components()
                            .filter(|c| *c != Component::CurDir)
                            .collect::<PathBuf>(),
                    ),
                );
            }
        }
        scope
    }
}
pub struct Draft {
    pub library: Library,
    original: Option<Vec<u8>>,
    bootstrap: Option<Library>,
    root: PathBuf,
}
impl Draft {
    fn check_local_overlap(library: &Library) -> Result<(), String> {
        let roots: Vec<_> = library
            .sources
            .iter()
            .filter(|s| s.remote.is_none())
            .filter_map(|s| fs::canonicalize(&s.path).ok().map(|p| (s, p)))
            .collect();
        for (i, (a, pa)) in roots.iter().enumerate() {
            for (b, pb) in roots.iter().skip(i + 1) {
                if pa.starts_with(pb) || pb.starts_with(pa) {
                    return Err(format!(
                        "Sources {} and {} resolve to overlapping folders",
                        a.name, b.name
                    ));
                }
            }
        }
        Ok(())
    }
    pub fn load(root: &Path) -> Result<Self, String> {
        let original = read(&path())?;
        let library = match &original {
            Some(bytes) => toml::from_str(std::str::from_utf8(bytes).map_err(|e| e.to_string())?)
                .map_err(|e| format!("sources configuration: {e}"))?,
            None => Library::bootstrap(root)?,
        };
        library.validate()?;
        let bootstrap = original.is_none().then(|| library.clone());
        Ok(Self {
            library,
            original,
            bootstrap,
            root: root.into(),
        })
    }
    pub fn add(&mut self, name: &str, path: &Path) -> Result<String, String> {
        if crate::batch::requires_remote(path)? {
            return Err("Choose Network source and its SSH server settings for this folder".into());
        }
        let path = fs::canonicalize(path).map_err(|e| format!("Cannot add folder: {e}"))?;
        if !path.is_dir() {
            return Err("Choose a directory".into());
        }
        if crate::batch::requires_remote(&path)? {
            return Err("Choose Network source and its SSH server settings for this folder".into());
        }
        self.register(name, path, None)
    }
    pub fn add_network(
        &mut self,
        name: &str,
        path: &Path,
        remote: Remote,
    ) -> Result<String, String> {
        // Do not stat or canonicalize a network mount: registering an offline server is allowed.
        remote.validate()?;
        let path: PathBuf = path
            .components()
            .filter(|c| *c != Component::CurDir)
            .collect();
        self.register(name, path, Some(remote))
    }
    fn register(
        &mut self,
        name: &str,
        path: PathBuf,
        remote: Option<Remote>,
    ) -> Result<String, String> {
        use std::io::Read;
        let mut random = [0u8; 8];
        fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut random))
            .map_err(|e| e.to_string())?;
        let id = format!("s{}", hex::encode(random));
        let mut library = self.library.clone();
        let anchor = if remote.is_none() {
            anchor(&path)
        } else {
            None
        };
        library.sources.push(Source {
            id: id.clone(),
            name: name.trim().into(),
            path,
            enabled: true,
            scope: Scope::default(),
            remote,
            anchor,
        });
        library.validate()?;
        Self::check_local_overlap(&library)?;
        self.library = library;
        Ok(id)
    }
    pub fn locate(&mut self, id: &str, path: &Path) -> Result<(), String> {
        if self.library.source(id)?.remote.is_some() {
            let mut library = self.library.clone();
            library
                .sources
                .iter_mut()
                .find(|s| s.id == id)
                .ok_or("Unknown source")?
                .path = path
                .components()
                .filter(|c| *c != Component::CurDir)
                .collect();
            library.validate()?;
            self.library = library;
            return Ok(());
        }
        if crate::batch::requires_remote(path)? {
            return Err("A local source cannot be relocated onto a network mount without server configuration".into());
        }
        let path = fs::canonicalize(path).map_err(|e| format!("Cannot locate folder: {e}"))?;
        if !path.is_dir() {
            return Err("Choose a directory".into());
        }
        let mut library = self.library.clone();
        let source = library
            .sources
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("Unknown source")?;
        if source.remote.is_none() && crate::batch::requires_remote(&path)? {
            return Err("A local source cannot be relocated onto a network mount without server configuration".into());
        }
        source.anchor = if source.remote.is_none() {
            anchor(&path)
        } else {
            None
        };
        source.path = path;
        library.validate()?;
        Self::check_local_overlap(&library)?;
        self.library = library;
        Ok(())
    }
    pub fn save(&mut self) -> Result<(), String> {
        self.library.validate()?;
        let path = path();
        let parent = path.parent().ok_or("No configuration directory")?;
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        let _lock = locking::sidecar(&path.with_extension("lock"))?;
        if read(&path)? != self.original {
            return Err("Sources changed in another window; reopen before saving".into());
        }
        // Compare bootstrap selection too: another old window may have changed library.toml.
        if self
            .bootstrap
            .as_ref()
            .is_some_and(|original| Library::bootstrap(&self.root).as_ref() != Ok(original))
        {
            return Err("Folder selection changed; reopen before saving".into());
        }
        let bytes = toml::to_string_pretty(&self.library)
            .map_err(|e| e.to_string())?
            .into_bytes();
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .map_err(|e| e.to_string())?;
            file.write_all(&bytes)
                .and_then(|_| file.sync_all())
                .map_err(|e| e.to_string())?;
            fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result?;
        self.original = Some(bytes);
        Ok(())
    }
}
impl Cfg {
    pub fn ensure_current(&self) -> Result<(), String> {
        if let Some(library) = self.library()? {
            if Library::load(&self.root)? != *library {
                return Err(
                    "Sources changed; reopen this window or restart the command before continuing"
                        .into(),
                );
            }
        }
        Ok(())
    }
    pub fn library(&self) -> Result<Option<&Library>, String> {
        self.sources
            .as_ref()
            .map(|r| r.as_ref().map_err(Clone::clone))
            .transpose()
    }
    pub fn file_path(&self, key: &str) -> Result<PathBuf, String> {
        match self.library()? {
            Some(library) => {
                let (source, rel) = library.identify(key)?;
                Ok(source.path.join(rel))
            }
            None => {
                relative(Path::new(key))?;
                Ok(self.root.join(key))
            }
        }
    }
    pub fn file_key(&self, path: &Path) -> Result<String, String> {
        match self.library()? {
            Some(library) => library.key(path),
            None => {
                let rel = path
                    .strip_prefix(&self.root)
                    .map_err(|_| "File is outside collection root")?;
                relative(rel)?;
                Ok(rel.to_string_lossy().into_owned())
            }
        }
    }
    pub fn for_source(&self, id: &str) -> Result<Self, String> {
        let library = self.library()?.ok_or("No source registry")?;
        let source = library.source(id)?;
        let mut c = self.clone();
        c.root = source.path.clone();
        c.active_source = Some(id.into());
        Ok(c)
    }
    pub fn for_file(&self, path: &Path) -> Result<Self, String> {
        self.ensure_current()?;
        if let Some(library) = self.library()? {
            if let Ok(key) = library.key(path) {
                let (source, _) = library.identify(&key)?;
                if !source.available() {
                    return Err("Source is offline or replaced; use Locate before editing".into());
                }
                return self.for_source(&source.id);
            }
            let mut c = self.clone();
            c.root = path.parent().ok_or("File has no parent")?.into();
            c.sources = None;
            c.active_source = None;
            return Ok(c);
        }
        Ok(self.clone())
    }
    pub fn key_for_scan(&self, scan: &mut index::Scan) {
        if let Some(id) = &self.active_source {
            scan.row.path = format!("{id}/{}", scan.row.path);
        }
    }
    pub fn scan_bytes(
        &self,
        path: &Path,
        bytes: &[u8],
        md: &fs::Metadata,
    ) -> Result<index::Scan, String> {
        if let Some(library) = self.library()? {
            let key = library.key(path)?;
            let (source, _) = library.identify(&key)?;
            let mut scan = index::scan_bytes(&source.path, path, bytes, md, &self.vocab);
            scan.row.path = key;
            Ok(scan)
        } else {
            Ok(index::scan_bytes(&self.root, path, bytes, md, &self.vocab))
        }
    }
}
pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let mut draft = Draft::load(&c.root)?;
    if args.first().is_some_and(|s| s == "add-network") {
        let name = args
            .get(1)
            .ok_or("add-network needs NAME MOUNTED_DIR and server options")?;
        let folder = args
            .get(2)
            .ok_or("add-network needs a mounted folder path")?;
        let mut options = std::collections::BTreeMap::new();
        for pair in args[3..].chunks(2) {
            if pair.len() != 2
                || !matches!(
                    pair[0].as_str(),
                    "--server" | "--host" | "--server-root" | "--pull-command" | "--batch-command"
                )
                || options.insert(pair[0].as_str(), pair[1].as_str()).is_some()
            {
                return Err("Invalid or repeated add-network option; use --server ID or --host HOST, --server-root DIR, --pull-command CMD, --batch-command CMD".into());
            }
        }
        if options.contains_key("--host") && options.contains_key("--server") {
            return Err("Choose --server ID or --host HOST".into());
        }
        let mut remote = if let Some(host) = options.get("--host") {
            Remote::defaults((*host).into(), PathBuf::new())
        } else {
            draft
                .library
                .source(options.get("--server").copied().unwrap_or("main"))?
                .remote
                .clone()
                .ok_or("That source has no SSH settings; use --host HOST")?
        };
        remote.root = options
            .get("--server-root")
            .copied()
            .unwrap_or(folder)
            .into();
        if let Some(command) = options.get("--pull-command") {
            remote.pull_command = (*command).into();
        }
        if let Some(command) = options.get("--batch-command") {
            remote.batch_command = (*command).into();
        }
        let id = draft.add_network(name, Path::new(folder), remote)?;
        draft.save()?;
        println!("Added source {id}");
        return run(&crate::cfg(), &[]);
    }
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["status"] => {},
        ["add", name, folder] => { let id = draft.add(name, Path::new(folder))?; draft.save()?; println!("Added source {id}"); },
        ["locate", id, folder] => { draft.locate(id, Path::new(folder))?; draft.save()?; },
        ["rename", id, name] => { draft.library.sources.iter_mut().find(|s| s.id == *id).ok_or("Unknown source")?.name = name.trim().into(); draft.save()?; },
        [operation @ ("enable" | "disable"), id] => { draft.library.sources.iter_mut().find(|s| s.id == *id).ok_or("Unknown source")?.enabled = *operation == "enable"; draft.save()?; },
        _ => return Err("usage: memetag sources [status|add NAME DIR|add-network NAME MOUNTED_DIR [--server ID | --host HOST] [--server-root DIR] [--pull-command CMD] [--batch-command CMD]|rename ID NAME|locate ID DIR|enable ID|disable ID]".into()),
    }
    for source in &draft.library.sources {
        let status = if source.remote.is_some() {
            "server configured"
        } else if source.available() {
            "available"
        } else {
            "offline or replaced folder (cache retained; use Locate)"
        };
        println!(
            "{}  {}  {}  {}  {}",
            source.id,
            source.name,
            if source.enabled {
                "enabled"
            } else {
                "disabled"
            },
            status,
            source.path.display()
        );
    }
    Ok(())
}

pub fn resolve_query(c: &Cfg, expr: &mut crate::query::Expr) -> Result<(), String> {
    use crate::query::Expr;
    match expr {
        Expr::Field { name, value, .. } if name == "source" => {
            let library = c
                .library()?
                .ok_or("Source filters require a source library")?;
            let source = library
                .sources
                .iter()
                .find(|s| s.id.eq_ignore_ascii_case(value))
                .or_else(|| {
                    library
                        .sources
                        .iter()
                        .find(|s| s.name.to_lowercase() == value.to_lowercase())
                })
                .ok_or_else(|| format!("Unknown source {value}"))?;
            *value = source.id.clone();
        }
        Expr::And(a, b) | Expr::Or(a, b) => {
            resolve_query(c, a)?;
            resolve_query(c, b)?;
        }
        Expr::Not(a) => resolve_query(c, a)?,
        _ => {}
    }
    Ok(())
}
