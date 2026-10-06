//! Keep the implication/alias rules — `~/.config/memetag/vocab.toml` — the same on every machine.
//!
//! Tags live in the files and the OCR text rides along in their XMP, so both already reach the server. Implications
//! and aliases do not: they are config, and the derived `implied` rows they produce are rebuilt into the SQLite index
//! from the *local* vocab.toml (`memetag reimply`, or a `pull` that hands the server its own copy per scan). So the
//! clients must synchronize rules: a `pull` from a client with stale rules would otherwise recompute
//! different derived rows.
//!
//! The fix, matching the rest of memetag ("the truth lives on the server, the index is a cache"): a **canonical
//! vocab.toml on the file server** at `$HOME/.local/lib/memetag/vocab.toml` (beside the pull/batch workers, user
//! owned). Every machine's local copy is a cache of it. Editing is two-way — either machine may push — with a
//! conflict guard so a push never silently clobbers rules the other machine made.
//!
//! Identity is a **fingerprint**: the SHA-256 of the normalized rule body (`Vocab::normalized_body`, the same sort
//! `save` writes). Comments, key order and formatting never move it; only the rules do. A one-line sidecar
//! `~/.config/memetag/vocab.sync` records the fingerprint both sides last agreed on — the *base*. From (local,
//! canonical, base) the direction is a table, not a guess:
//!
//! | local vs base | canonical vs base | action                                   |
//! |---------------|-------------------|------------------------------------------|
//! | canonical absent                  || **seed**: push local up                  |
//! | local == canonical                || record agreement, nothing to write       |
//! | changed       | unchanged         | **push** local → canonical               |
//! | unchanged     | changed           | **pull** canonical → local, then reimply |
//! | changed       | changed (differ)  | **conflict** — never auto-overwrite       |
//! | no base, and they differ          || **conflict** — direction is unknowable    |
//!
//! On a conflict nothing on disk or on the server changes: a terminal prints the differing rules and waits for
//! `--take-mine` / `--take-theirs` / `--merge`; the Implications card offers the same three, merge included.
//!
//! The transport is a trait so the tests run against a local directory, never ssh.

use crate::vocab::Vocab;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The canonical rule file on the server, behind a trait so tests use a plain local file instead of ssh.
pub trait Canonical {
    /// The stored bytes, or `None` when nothing has been stored yet (first ever sync).
    fn fetch(&self) -> Result<Option<Vec<u8>>, String>;
    /// Replace only if the canonical bytes still match the fetched snapshot (`None` means absent).
    /// The comparison and replacement must share a server-side lock.
    fn store(&self, expected: Option<&[u8]>, bytes: &[u8]) -> Result<(), String>;
    /// A short name for messages ("user@host").
    fn name(&self) -> String;
}

/// The path the canonical vocab lives at on the server, resolved on the far side so `$HOME` is the server's home.
/// Not user input — a fixed literal — so embedding it in a double-quoted shell word is safe.
const REMOTE_PATH: &str = "$HOME/.local/lib/memetag/vocab.toml";

fn store_command(path: &str, expected: Option<&[u8]>, bytes: &[u8]) -> String {
    let expected = expected
        .map(|b| format!("{:x}", Sha256::digest(b)))
        .unwrap_or_else(|| "absent".into());
    let incoming = format!("{:x}", Sha256::digest(bytes));
    // All interpolated values are either validated paths or hex digests. Read stdin completely before
    // comparing, so a failed/partial upload cannot publish. Never unlink the stable lock file.
    format!(
        r#"set -eu
p="{path}"
mkdir -p "$(dirname "$p")"
t=$(mktemp "$p.tmp.XXXXXXXX")
trap 'rm -f "$t"' EXIT HUP INT TERM
cat > "$t"
got=$(sha256sum < "$t"); got=${{got%% *}}
[ "$got" = '{incoming}' ] || {{ echo 'incomplete vocabulary upload' >&2; exit 46; }}
exec 9>"$p.lock"
flock -x -w 10 9 || exit 47
if [ -e "$p" ]; then actual=$(sha256sum < "$p"); actual=${{actual%% *}}; else actual=absent; fi
[ "$actual" = '{expected}' ] || {{ echo 'canonical rules changed during sync; retry to review the new conflict' >&2; exit 45; }}
mv -f "$t" "$p"
"#
    )
}

/// The canonical path, overridable by `MEMETAG_VOCAB_REMOTE` (an isolated path for tests, or a server whose layout
/// differs), matching the `MEMETAG_ROOT`/`MEMETAG_DB` overrides elsewhere. The value is trusted config, embedded in a
/// double-quoted shell word, so a would-be `"`/`$`/`` ` `` is refused rather than run.
fn remote_path() -> String {
    match std::env::var("MEMETAG_VOCAB_REMOTE") {
        Ok(p) if !p.is_empty() && !p.contains(['"', '`', '$', '\\']) => p,
        _ => REMOTE_PATH.to_string(),
    }
}

/// A transport error carries this tag when ssh could not reach the host at all (exit 255: refused, timed out, no route,
/// auth rejected), as opposed to a failure once connected. The pull step reads it to skip its own connect to the same
/// host instead of paying a second timeout when the server is asleep (2026-09-26).
pub const UNREACHABLE_TAG: &str = "[unreachable]";
pub fn is_unreachable(err: &str) -> bool {
    err.contains(UNREACHABLE_TAG)
}
/// The message for an ssh that returned 255, tagged so `is_unreachable` catches it.
fn unreachable_err(host: &str, stderr: &str) -> String {
    format!("{UNREACHABLE_TAG} could not reach {host}: {stderr}")
}

/// The server copy over a plain ssh (`ssh host cat`), separate from the pull worker's JSON session so no server
/// binary has to change. Absent file is signalled by exit code 44, told apart from a real ssh failure.
pub struct Ssh {
    pub host: String,
    /// seconds ssh may take to connect; the grab path passes a short one so a sleeping server never stalls the window
    pub connect_timeout: u32,
}
impl Ssh {
    fn ssh(&self, remote_cmd: &str) -> Command {
        crate::remote::ssh("ssh", &self.host, remote_cmd, self.connect_timeout)
    }
}
impl Canonical for Ssh {
    fn fetch(&self) -> Result<Option<Vec<u8>>, String> {
        let cmd = format!(
            "p=\"{}\"; if [ -f \"$p\" ]; then cat \"$p\"; else exit 44; fi",
            remote_path()
        );
        let out = self
            .ssh(&cmd)
            .output()
            .map_err(|e| format!("ssh (fetch vocab): {e}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        match out.status.code() {
            Some(0) => Ok(Some(out.stdout)),
            Some(44) => Ok(None),
            Some(255) => Err(unreachable_err(&self.host, stderr.trim())),
            _ => Err(format!(
                "fetch canonical vocab from {}: {}",
                self.host,
                stderr.trim()
            )),
        }
    }
    fn store(&self, expected: Option<&[u8]>, bytes: &[u8]) -> Result<(), String> {
        let cmd = store_command(&remote_path(), expected, bytes);
        let mut child = self
            .ssh(&cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("ssh (store vocab): {e}"))?;
        let sent = child
            .stdin
            .take()
            .ok_or("ssh stdin")?
            .write_all(bytes)
            .map_err(|e| format!("send vocab to {}: {e}", self.host));
        let out = child
            .wait_with_output()
            .map_err(|e| format!("ssh (store vocab): {e}"))?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if out.status.success() {
            sent
        } else if out.status.code() == Some(255) {
            Err(unreachable_err(&self.host, stderr.trim()))
        } else {
            Err(format!(
                "store canonical vocab on {}: {}",
                self.host,
                stderr.trim()
            ))
        }
    }
    fn name(&self) -> String {
        self.host.clone()
    }
}

/// The SHA-256 of the normalized rule body: identical rule sets fingerprint identically whatever their formatting.
pub fn fingerprint(v: &Vocab) -> String {
    let mut h = Sha256::new();
    h.update(v.normalized_body().as_bytes());
    format!("{:x}", h.finalize())
}

/// Local file locations, from `$HOME` exactly as `main::cfg` computes them.
pub struct Env {
    pub vocab_path: PathBuf,
    pub base_path: PathBuf,
}
impl Env {
    pub fn from_home() -> Env {
        let dir = crate::paths::config_dir();
        Env {
            vocab_path: dir.join("vocab.toml"),
            base_path: dir.join("vocab.sync"),
        }
    }
    fn read_base(&self) -> Option<String> {
        let s = std::fs::read_to_string(&self.base_path).ok()?;
        let fp = s.trim();
        (fp.len() == 64 && fp.bytes().all(|b| b.is_ascii_hexdigit())).then(|| fp.to_string())
    }
    fn write_base(&self, fp: &str) -> Result<(), String> {
        let tmp = self.base_path.with_extension("sync.tmp");
        std::fs::write(&tmp, format!("{fp}\n"))
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.base_path)
            .map_err(|e| format!("replace {}: {e}", self.base_path.display()))
    }
    fn write_local(&self, bytes: &[u8]) -> Result<(), String> {
        let tmp = self.vocab_path.with_extension("toml.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.vocab_path)
            .map_err(|e| format!("replace {}: {e}", self.vocab_path.display()))
    }
}

/// The direction the table picks; a pure function of the three fingerprints, so it is unit-tested on its own.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Decision {
    Seed,
    InSync,
    Push,
    Pull,
    Conflict,
}
pub fn decide(local_fp: &str, canon_fp: Option<&str>, base: Option<&str>) -> Decision {
    let Some(canon_fp) = canon_fp else {
        return Decision::Seed;
    };
    if local_fp == canon_fp {
        return Decision::InSync;
    }
    match base {
        None => Decision::Conflict,
        Some(b) => match (local_fp != b, canon_fp != b) {
            (true, false) => Decision::Push,
            (false, true) => Decision::Pull,
            // both moved, or the impossible base==local==canon while local!=canon: never overwrite blind
            _ => Decision::Conflict,
        },
    }
}

/// What actually happened, for the caller to report and to decide whether a reimply is due.
#[derive(Debug)]
pub enum Outcome {
    InSync,
    Seeded,
    Pushed,
    /// local vocab.toml was replaced from the server; the caller must reimply so the index catches up
    Pulled,
    Conflict(Conflict),
}

/// A stopped sync: both sides moved. Carries enough to show the difference and to resolve it any of the three ways.
#[derive(Debug, Clone)]
pub struct Conflict {
    pub local: Vocab,
    pub canon: Vocab,
    pub diff: Diff,
}

/// The rules and aliases that differ between two vocabularies, each as a human line, for conflict and status display.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diff {
    pub only_local: Vec<String>,
    pub only_canon: Vec<String>,
}
/// Every rule and alias of a vocabulary as sorted display lines ("a ⇒ b", "x = y"), the unit the diff compares.
fn lines(v: &Vocab) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (from, to) in v.rules() {
        out.insert(format!("{from} ⇒ {to}"));
    }
    let mut aliases: Vec<(&String, &String)> = v.aliases.iter().collect();
    aliases.sort();
    for (k, target) in aliases {
        out.insert(format!("{k} = {target}"));
    }
    out
}
pub fn diff(local: &Vocab, canon: &Vocab) -> Diff {
    let (l, c) = (lines(local), lines(canon));
    Diff {
        only_local: l.difference(&c).cloned().collect(),
        only_canon: c.difference(&l).cloned().collect(),
    }
}

/// An alias both sides define but disagree on: the one thing a union merge cannot settle on its own.
#[derive(Debug, PartialEq, Eq)]
pub struct AliasClash {
    pub key: String,
    pub local: String,
    pub canon: String,
}
/// The union of two vocabularies: every implication of either (target lists unioned — implications never truly clash),
/// every alias of either. Aliases whose key both sides map to *different* targets are returned unresolved, not guessed.
/// Local wins the key in the returned vocabulary until the caller resolves it, so a merge with clashes is not applied.
pub fn merge(local: &Vocab, canon: &Vocab) -> (Vocab, Vec<AliasClash>) {
    let mut out = local.clone();
    for (from, targets) in &canon.implications {
        for t in targets {
            out.add_rule(from, t);
        }
    }
    let mut clashes = Vec::new();
    for (k, target) in &canon.aliases {
        match out.aliases.get(k) {
            Some(existing) if existing != target => clashes.push(AliasClash {
                key: k.clone(),
                local: existing.clone(),
                canon: target.clone(),
            }),
            Some(_) => {}
            None => {
                out.aliases.insert(k.clone(), target.clone());
            }
        }
    }
    clashes.sort_by(|a, b| a.key.cmp(&b.key));
    (out, clashes)
}

/// Read the local vocab and the canonical bytes, then act on the table. On a conflict nothing is written.
pub fn sync(store: &dyn Canonical, env: &Env) -> Result<Outcome, String> {
    let _lock = Vocab::lock(&env.vocab_path)?;
    let local = Vocab::load(&env.vocab_path);
    if let Some(e) = &local.broken {
        return Err(format!(
            "{} did not parse ({e}); fix it before syncing, or a push would carry the damage to every machine",
            env.vocab_path.display()
        ));
    }
    let local_bytes = local.original.clone().unwrap_or_default();
    let local_fp = fingerprint(&local);

    let canon_bytes = store.fetch()?;
    let canon: Option<Vocab> = match &canon_bytes {
        Some(b) => {
            let text = String::from_utf8_lossy(b);
            let v: Vocab = toml::from_str(&text).map_err(|e| {
                format!(
                    "the canonical vocab on {} did not parse ({e}); fix it there",
                    store.name()
                )
            })?;
            Some(v)
        }
        None => None,
    };
    let canon_fp = canon.as_ref().map(fingerprint);
    let base = env.read_base();

    match decide(&local_fp, canon_fp.as_deref(), base.as_deref()) {
        Decision::Seed => {
            store.store(canon_bytes.as_deref(), &local_bytes)?;
            env.write_base(&local_fp)?;
            Ok(Outcome::Seeded)
        }
        Decision::InSync => {
            env.write_base(&local_fp)?; // record the agreement so a later one-sided edit is unambiguous
            Ok(Outcome::InSync)
        }
        Decision::Push => {
            store.store(canon_bytes.as_deref(), &local_bytes)?;
            env.write_base(&local_fp)?;
            Ok(Outcome::Pushed)
        }
        Decision::Pull => {
            let bytes = canon_bytes.expect("Pull only when canonical is present");
            env.write_local(&bytes)?;
            env.write_base(canon_fp.as_deref().expect("canon_fp present with canon"))?;
            Ok(Outcome::Pulled)
        }
        Decision::Conflict => {
            let canon = canon.expect("Conflict only when canonical is present");
            let d = diff(&local, &canon);
            Ok(Outcome::Conflict(Conflict {
                local,
                canon,
                diff: d,
            }))
        }
    }
}

/// A read-only description of what a sync would do, for `vocab status`. Writes nothing on disk or the server.
pub fn sync_dry(store: &dyn Canonical, env: &Env) -> Result<String, String> {
    let local = Vocab::load(&env.vocab_path);
    if let Some(e) = &local.broken {
        return Ok(format!(
            "local {} does not parse ({e}); fix it before syncing",
            env.vocab_path.display()
        ));
    }
    let local_fp = fingerprint(&local);
    let canon_bytes = store.fetch()?;
    let canon: Option<Vocab> = match &canon_bytes {
        Some(b) => Some(toml::from_str(&String::from_utf8_lossy(b)).map_err(|e| {
            format!(
                "the canonical vocab on {} did not parse ({e})",
                store.name()
            )
        })?),
        None => None,
    };
    let canon_fp = canon.as_ref().map(fingerprint);
    let base = env.read_base();
    let (la, li) = (local.aliases.len(), local.rules().len());
    Ok(match decide(&local_fp, canon_fp.as_deref(), base.as_deref()) {
        Decision::Seed => format!(
            "{} holds no canonical vocab yet; a sync would seed it from your {la} aliases and {li} implications",
            store.name()
        ),
        Decision::InSync => {
            format!("in sync with {}: {la} aliases, {li} implications", store.name())
        }
        Decision::Push => format!(
            "your rules are ahead of {}; a sync would push them ({la} aliases, {li} implications)",
            store.name()
        ),
        Decision::Pull => format!(
            "{} is ahead; a sync would pull its rules and reapply them to the index",
            store.name()
        ),
        Decision::Conflict => {
            let canon = canon.expect("a conflict implies the canonical is present");
            let d = diff(&local, &canon);
            let mut s = format!(
                "CONFLICT with {}: both sides changed since the last sync.\n",
                store.name()
            );
            for l in &d.only_local {
                s.push_str(&format!("  only here:   {l}\n"));
            }
            for l in &d.only_canon {
                s.push_str(&format!("  only server: {l}\n"));
            }
            s.push_str("  resolve: memetag vocab sync --take-mine | --take-theirs | --merge");
            s
        }
    })
}

/// Resolve a conflict by keeping the local rules: push them and adopt them as the base.
pub fn take_mine(store: &dyn Canonical, env: &Env) -> Result<(), String> {
    let _lock = Vocab::lock(&env.vocab_path)?;
    let local = Vocab::load(&env.vocab_path);
    if let Some(e) = &local.broken {
        return Err(format!("{} did not parse ({e})", env.vocab_path.display()));
    }
    let bytes = local.original.clone().unwrap_or_default();
    let expected = store.fetch()?;
    store.store(expected.as_deref(), &bytes)?;
    env.write_base(&fingerprint(&local))
}

/// Resolve a conflict by taking the server's rules: overwrite the local file and adopt them as the base. The caller
/// must reimply afterwards.
pub fn take_theirs(store: &dyn Canonical, env: &Env) -> Result<(), String> {
    let _lock = Vocab::lock(&env.vocab_path)?;
    let bytes = store
        .fetch()?
        .ok_or("the server has no canonical vocab to take")?;
    let canon: Vocab = toml::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|e| format!("the canonical vocab did not parse ({e})"))?;
    env.write_local(&bytes)?;
    env.write_base(&fingerprint(&canon))
}

/// Resolve a conflict by merging: save the union to the local file, push it, adopt it as the base. `resolved` maps
/// each clashing alias key to the target the user picked; any clash left unresolved keeps the local target. The
/// caller must reimply afterwards.
pub fn resolve_merge(
    store: &dyn Canonical,
    env: &Env,
    resolved: &[(String, String)],
) -> Result<Vec<AliasClash>, String> {
    let _lock = Vocab::lock(&env.vocab_path)?;
    let (local, bytes, canon) = both_sides(store, env)?;
    let (mut merged, clashes) = merge(&local, &canon);
    if clashes.iter().any(|c| {
        !resolved
            .iter()
            .any(|(k, t)| k == &c.key && (t == &c.local || t == &c.canon))
    }) {
        return Err("Alias conflicts changed or remain unresolved; review the merge again. Nothing was written.".into());
    }
    for (k, target) in resolved {
        if !clashes
            .iter()
            .any(|c| &c.key == k && (&c.local == target || &c.canon == target))
        {
            return Err(
                "Alias choices are stale; review the merge again. Nothing was written.".into(),
            );
        }
        merged.aliases.insert(k.clone(), target.clone());
    }
    let saved = merged.normalized_body().into_bytes();
    // Server first: a failed compare-and-swap must leave both local rules and base untouched.
    store.store(Some(&bytes), &saved)?;
    env.write_local(&saved)?;
    env.write_base(&fingerprint(&merged))?;
    // report only the clashes the caller did not resolve, so it can tell whether the merge was clean
    Ok(clashes
        .into_iter()
        .filter(|c| !resolved.iter().any(|(k, _)| *k == c.key))
        .collect())
}

/// The union without writing anything, so a caller can show the clashes and decide before committing.
pub fn preview_merge(store: &dyn Canonical, env: &Env) -> Result<(Vocab, Vec<AliasClash>), String> {
    let (local, _, canon) = both_sides(store, env)?;
    Ok(merge(&local, &canon))
}

/// The local file (a broken one is refused, never merged over) and the server's canonical copy, which comes back
/// parsed and as the bytes a later compare-and-swap must present.
fn both_sides(store: &dyn Canonical, env: &Env) -> Result<(Vocab, Vec<u8>, Vocab), String> {
    let local = Vocab::load(&env.vocab_path);
    if let Some(e) = &local.broken {
        return Err(format!("{} did not parse ({e})", env.vocab_path.display()));
    }
    let bytes = store
        .fetch()?
        .ok_or("the server has no canonical vocab to merge")?;
    let canon: Vocab = toml::from_str(&String::from_utf8_lossy(&bytes))
        .map_err(|e| format!("the canonical vocab did not parse ({e})"))?;
    Ok((local, bytes, canon))
}

/// The store for the configured server, or `None` when the root is local (nothing to sync against).
pub fn store_for(c: &crate::Cfg) -> Result<Option<Ssh>, String> {
    // Vocabulary is shared by the whole library, with one authority: the original main server.
    if let Some(library) = c.library()? {
        return Ok(library.source("main")?.remote.as_ref().map(|r| Ssh {
            host: r.host.clone(),
            connect_timeout: 10,
        }));
    }
    Ok(crate::pull::remote_host(c)?.map(|host| Ssh {
        host,
        connect_timeout: 10,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::Path;

    #[test]
    fn additional_servers_keep_the_main_vocabulary_authority() {
        use crate::sources::{Library, Remote, Source};
        let source = |id: &str, host: &str| Source {
            id: id.into(),
            name: id.into(),
            path: PathBuf::from(format!("/mounted/{id}")),
            enabled: true,
            scope: crate::library::Scope::default(),
            anchor: None,
            remote: Some(Remote::defaults(
                host.into(),
                format!("/server/{id}").into(),
            )),
        };
        let mut c = crate::cfg();
        c.sources = Some(Ok(Library {
            sources: vec![source("main", "primary"), source("other", "secondary")],
        }));
        c.active_source = Some("other".into());
        assert_eq!(store_for(&c).unwrap().unwrap().host, "primary");
        c.sources.as_mut().unwrap().as_mut().unwrap().sources[0].remote = None;
        assert!(store_for(&c).unwrap().is_none());
    }

    /// A canonical store backed by an in-memory slot, so the decision logic is tested without ssh.
    struct Mem {
        slot: RefCell<Option<Vec<u8>>>,
    }
    impl Mem {
        fn new(init: Option<&str>) -> Mem {
            Mem {
                slot: RefCell::new(init.map(|s| s.as_bytes().to_vec())),
            }
        }
        fn text(&self) -> Option<String> {
            self.slot
                .borrow()
                .as_ref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
        }
    }
    impl Canonical for Mem {
        fn fetch(&self) -> Result<Option<Vec<u8>>, String> {
            Ok(self.slot.borrow().clone())
        }
        fn store(&self, expected: Option<&[u8]>, bytes: &[u8]) -> Result<(), String> {
            if self.slot.borrow().as_deref() != expected {
                return Err("canonical rules changed during sync".into());
            }
            *self.slot.borrow_mut() = Some(bytes.to_vec());
            Ok(())
        }
        fn name(&self) -> String {
            "mem".into()
        }
    }

    fn env_in(dir: &Path) -> Env {
        Env {
            vocab_path: dir.join("vocab.toml"),
            base_path: dir.join("vocab.sync"),
        }
    }
    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "memetag-vsync-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    fn write(env: &Env, body: &str) {
        std::fs::write(&env.vocab_path, body).unwrap();
    }

    const A: &str =
        "[aliases]\n\"lol\" = \"reaction:laugh\"\n[implications]\n\"reimu\" = [\"touhou\"]\n";
    const B: &str = "[aliases]\n\"lol\" = \"reaction:laugh\"\n[implications]\n\"reimu\" = [\"touhou\"]\n\"marisa\" = [\"touhou\"]\n";

    struct Racing {
        inner: Mem,
    }
    impl Canonical for Racing {
        fn fetch(&self) -> Result<Option<Vec<u8>>, String> {
            let observed = self.inner.fetch()?;
            *self.inner.slot.borrow_mut() = Some(B.as_bytes().to_vec());
            Ok(observed)
        }
        fn store(&self, expected: Option<&[u8]>, bytes: &[u8]) -> Result<(), String> {
            self.inner.store(expected, bytes)
        }
        fn name(&self) -> String {
            "racing server".into()
        }
    }

    #[test]
    fn concurrent_server_edit_survives_seed_push_and_merge() {
        for initial in [None, Some(A)] {
            let dir = tmpdir(if initial.is_none() {
                "race-seed"
            } else {
                "race-push"
            });
            let env = env_in(&dir);
            let local = "[implications]\nlocal = ['addition']\n";
            write(&env, local);
            let base = fingerprint(&toml::from_str::<Vocab>(A).unwrap());
            env.write_base(&base).unwrap();
            let store = Racing {
                inner: Mem::new(initial),
            };
            assert!(sync(&store, &env).unwrap_err().contains("changed"));
            assert_eq!(store.inner.text().as_deref(), Some(B));
            assert_eq!(std::fs::read_to_string(&env.vocab_path).unwrap(), local);
            assert_eq!(env.read_base().as_deref(), Some(base.as_str()));
            *store.inner.slot.borrow_mut() = Some(A.as_bytes().to_vec());
            assert!(resolve_merge(&store, &env, &[])
                .unwrap_err()
                .contains("changed"));
            assert_eq!(store.inner.text().as_deref(), Some(B));
            assert_eq!(std::fs::read_to_string(&env.vocab_path).unwrap(), local);
            assert_eq!(env.read_base().as_deref(), Some(base.as_str()));
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn unresolved_merge_and_busy_local_rules_write_nothing() {
        let dir = tmpdir("merge-guard");
        let env = env_in(&dir);
        write(&env, "[aliases]\nx = 'local'\n");
        let store = Mem::new(Some("[aliases]\nx = 'server'\n"));
        let before = std::fs::read(&env.vocab_path).unwrap();
        assert!(resolve_merge(&store, &env, &[]).is_err());
        assert_eq!(std::fs::read(&env.vocab_path).unwrap(), before);
        assert_eq!(store.text().as_deref(), Some("[aliases]\nx = 'server'\n"));
        let lock = Vocab::lock(&env.vocab_path).unwrap();
        assert!(sync(&store, &env).unwrap_err().contains("busy"));
        drop(lock);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transport_script_checks_revision_and_complete_upload() {
        let dir = tmpdir("transport");
        let path = dir.join("rules with spaces.toml");
        let run = |expected: Option<&[u8]>, declared: &[u8], sent: &[u8]| {
            let mut child = Command::new("sh")
                .arg("-c")
                .arg(store_command(path.to_str().unwrap(), expected, declared))
                .stdin(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(sent).unwrap();
            child.wait_with_output().unwrap().status
        };
        assert!(run(None, A.as_bytes(), A.as_bytes()).success());
        assert_eq!(run(None, B.as_bytes(), B.as_bytes()).code(), Some(45));
        assert!(run(Some(A.as_bytes()), B.as_bytes(), B.as_bytes()).success());
        assert_eq!(
            run(Some(A.as_bytes()), A.as_bytes(), A.as_bytes()).code(),
            Some(45)
        );
        assert_eq!(
            run(Some(B.as_bytes()), A.as_bytes(), b"partial").code(),
            Some(46)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), B);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "only canonical and permanent lock remain"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fingerprint_ignores_formatting_and_order() {
        let one: Vocab = toml::from_str(
            "[implications]\n\"reimu\" = [\"touhou\", \"witch\"]\n\"marisa\" = [\"touhou\"]\n",
        )
        .unwrap();
        let two: Vocab = toml::from_str(
            "# a comment\n[implications]\n\"marisa\" = [\"touhou\"]\n\"reimu\" = [\"witch\", \"touhou\"]\n",
        )
        .unwrap();
        assert_eq!(fingerprint(&one), fingerprint(&two));
        let three: Vocab = toml::from_str("[implications]\n\"reimu\" = [\"touhou\"]\n").unwrap();
        assert_ne!(fingerprint(&one), fingerprint(&three));
    }

    #[test]
    fn decision_table() {
        assert_eq!(decide("l", None, None), Decision::Seed);
        assert_eq!(decide("x", Some("x"), None), Decision::InSync);
        assert_eq!(decide("new", Some("base"), Some("base")), Decision::Push);
        assert_eq!(decide("base", Some("new"), Some("base")), Decision::Pull);
        assert_eq!(decide("l", Some("c"), Some("base")), Decision::Conflict);
        assert_eq!(decide("l", Some("c"), None), Decision::Conflict);
    }

    #[test]
    fn seed_then_insync() {
        let dir = tmpdir("seed");
        let env = env_in(&dir);
        write(&env, A);
        let store = Mem::new(None);
        assert!(matches!(sync(&store, &env).unwrap(), Outcome::Seeded));
        assert_eq!(store.text().unwrap(), A);
        // a second sync with nothing changed is a no-op InSync
        assert!(matches!(sync(&store, &env).unwrap(), Outcome::InSync));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn push_when_only_local_moved() {
        let dir = tmpdir("push");
        let env = env_in(&dir);
        write(&env, A);
        let store = Mem::new(None);
        sync(&store, &env).unwrap(); // seed A, base = fp(A)
        write(&env, B); // local grows a rule
        assert!(matches!(sync(&store, &env).unwrap(), Outcome::Pushed));
        assert_eq!(store.text().unwrap(), B);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pull_when_only_canon_moved() {
        let dir = tmpdir("pull");
        let env = env_in(&dir);
        write(&env, A);
        let store = Mem::new(None);
        sync(&store, &env).unwrap(); // seed A, base = fp(A)
        store.store(Some(A.as_bytes()), B.as_bytes()).unwrap(); // server grows a rule (as if from another machine)
        let out = sync(&store, &env).unwrap();
        assert!(
            matches!(out, Outcome::Pulled),
            "a pull leaves the index to rebuild"
        );
        assert_eq!(std::fs::read_to_string(&env.vocab_path).unwrap(), B);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn conflict_when_both_moved_and_take_theirs() {
        let dir = tmpdir("conflict");
        let env = env_in(&dir);
        write(&env, A);
        let store = Mem::new(None);
        sync(&store, &env).unwrap(); // base = fp(A)
                                     // both sides diverge from A, differently
        write(&env, B); // local adds marisa
        let server = "[aliases]\n\"lol\" = \"reaction:laugh\"\n[implications]\n\"reimu\" = [\"touhou\"]\n\"sakuya\" = [\"touhou\"]\n";
        store.store(Some(A.as_bytes()), server.as_bytes()).unwrap();
        let out = sync(&store, &env).unwrap();
        let Outcome::Conflict(c) = out else {
            panic!("expected a conflict")
        };
        assert!(c.diff.only_local.iter().any(|l| l.contains("marisa")));
        assert!(c.diff.only_canon.iter().any(|l| l.contains("sakuya")));
        // nothing was written on a conflict
        assert_eq!(std::fs::read_to_string(&env.vocab_path).unwrap(), B);
        assert_eq!(store.text().unwrap(), server);
        // take theirs: local becomes the server copy, base follows
        take_theirs(&store, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&env.vocab_path).unwrap(), server);
        assert!(matches!(sync(&store, &env).unwrap(), Outcome::InSync));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_unions_rules_and_flags_alias_clashes() {
        let local: Vocab = toml::from_str(
            "[aliases]\n\"pg\" = \"peter griffin\"\n[implications]\n\"reimu\" = [\"touhou\"]\n",
        )
        .unwrap();
        let canon: Vocab = toml::from_str(
            "[aliases]\n\"pg\" = \"peter griffon\"\n[implications]\n\"marisa\" = [\"touhou\"]\n",
        )
        .unwrap();
        let (merged, clashes) = merge(&local, &canon);
        assert!(merged.implications.contains_key("reimu"));
        assert!(merged.implications.contains_key("marisa"));
        assert_eq!(clashes.len(), 1);
        assert_eq!(clashes[0].key, "pg");
        assert_eq!(clashes[0].local, "peter griffin");
        assert_eq!(clashes[0].canon, "peter griffon");
        // the unresolved clash keeps the local target in the merged vocab, never a guess
        assert_eq!(merged.aliases["pg"], "peter griffin");
    }

    #[test]
    fn merge_resolution_applies_the_picked_target() {
        let dir = tmpdir("merge");
        let env = env_in(&dir);
        write(
            &env,
            "[aliases]\n\"pg\" = \"peter griffin\"\n[implications]\n\"reimu\" = [\"touhou\"]\n",
        );
        let store = Mem::new(Some(
            "[aliases]\n\"pg\" = \"peter griffon\"\n[implications]\n\"marisa\" = [\"touhou\"]\n",
        ));
        // resolve the pg clash toward the server's spelling
        let left = resolve_merge(&store, &env, &[("pg".into(), "peter griffon".into())]).unwrap();
        assert!(left.is_empty(), "the one clash was resolved");
        let after = Vocab::load(&env.vocab_path);
        assert_eq!(after.aliases["pg"], "peter griffon");
        assert!(after.implications.contains_key("reimu"));
        assert!(after.implications.contains_key("marisa"));
        // server now equals local, base recorded → next sync is clean
        assert!(matches!(sync(&store, &env).unwrap(), Outcome::InSync));
        std::fs::remove_dir_all(&dir).ok();
    }
}
