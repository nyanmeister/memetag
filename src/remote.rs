//! Shared source routing and SSH options. Pull, editing and viewing retain
//! separate configuration policies and their own worker protocols.
use crate::Cfg;
use serde::Deserialize;
use std::{ffi::OsStr, path::PathBuf, process::Command};

#[derive(Deserialize)]
pub(crate) struct Remote {
    pub local_root: PathBuf,
    pub root: PathBuf,
    pub host: String,
    pub command: String,
}

#[derive(Clone, Copy)]
pub(crate) enum Purpose {
    Pull,
    Edit,
    View,
}

/// Prefer the source registry; legacy sections apply only to their matching root.
/// Viewing can use a pull-only server, but editing must have a writer configured.
pub(crate) fn configured(c: &Cfg, purpose: Purpose) -> Result<Option<Remote>, String> {
    if let Some(library) = c.library()? {
        let source = library.source(c.active_source.as_deref().unwrap_or("main"))?;
        if let Some(remote) = &source.remote {
            return Ok(Some(Remote {
                local_root: source.path.clone(),
                root: remote.root.clone(),
                host: remote.host.clone(),
                command: match purpose {
                    Purpose::Pull => &remote.pull_command,
                    Purpose::Edit | Purpose::View => &remote.batch_command,
                }
                .clone(),
            }));
        }
    }
    let (sections, label, kind): (&[&str], _, _) = match purpose {
        Purpose::Pull => (&["pull_remote"], "Pull", "pull"),
        Purpose::Edit => (&["batch_remote"], "Batch", "batch"),
        Purpose::View => (&["pull_remote", "batch_remote"], "Batch", "batch"),
    };
    let table = crate::paths::config_table().map_err(|e| format!("{label} configuration: {e}"))?;
    for value in sections.iter().filter_map(|section| table.get(*section)) {
        let remote: Remote = value
            .clone()
            .try_into()
            .map_err(|e| format!("{label} configuration: {e}"))?;
        if c.root == remote.local_root {
            if remote.host.is_empty()
                || remote.host.starts_with('-')
                || remote.host.chars().any(char::is_whitespace)
            {
                return Err(format!("Invalid {kind} server host"));
            }
            return Ok(Some(remote));
        }
    }
    if crate::batch::requires_remote(&c.root)? {
        return Err(match purpose {
            Purpose::Pull => "Pulling this share needs a matching [pull_remote] configuration — never walk the mount",
            Purpose::Edit | Purpose::View => "Editing a network collection needs a matching batch_remote configuration",
        }.into());
    }
    Ok(None)
}

/// Only connection options are shared: each caller owns stdio, protocol and
/// retry decisions. Paths/tags must still travel as JSON or safely quoted input.
pub(crate) fn ssh(
    executable: impl AsRef<OsStr>,
    host: &str,
    command: &str,
    connect_timeout_secs: u32,
) -> Command {
    let mut ssh = Command::new(executable);
    ssh.args([
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={connect_timeout_secs}"),
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
        host,
        command,
    ]);
    ssh
}

/// A worker's loop: one JSON request per stdin line, one JSON `Result` per stdout line, flushed, until EOF.
/// A line that is not a request is answered with the parse error, so the client's protocol never stalls.
pub(crate) fn serve_lines<Req: serde::de::DeserializeOwned, Rep: serde::Serialize>(
    handle: impl Fn(Req) -> Result<Rep, String>,
) -> Result<(), String> {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let result = serde_json::from_str::<Req>(&line)
            .map_err(|e| e.to_string())
            .and_then(&handle);
        serde_json::to_writer(&mut stdout, &result).map_err(|e| e.to_string())?;
        writeln!(stdout).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// One request over a worker's pipes: the JSON line out, the JSON line back. `Ok(None)` is no answer: the pipe
/// broke or the worker closed its end (an ssh that was refused or timed out, a helper that exited); the caller
/// names the cause from the child's exit status. `Err` is an answer that was not JSON.
pub(crate) fn exchange<Req: serde::Serialize, Rep: serde::de::DeserializeOwned>(
    input: &mut impl std::io::Write,
    output: &mut impl std::io::BufRead,
    req: &Req,
    what: &str,
) -> Result<Option<Rep>, String> {
    let mut line = String::new();
    let sent = serde_json::to_writer(&mut *input, req)
        .map_err(|e| e.to_string())
        .and_then(|_| writeln!(input).map_err(|e| e.to_string()))
        .and_then(|_| input.flush().map_err(|e| e.to_string()))
        .and_then(|_| output.read_line(&mut line).map_err(|e| e.to_string()));
    match sent {
        Ok(0) | Err(_) => Ok(None),
        Ok(_) => serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| format!("{what} response: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn exchange_sends_one_line_and_reads_one_answer() {
        let mut sent = Vec::new();
        let mut answers = Cursor::new(b"{\"Ok\":7}\n{\"Err\":\"no\"}\nnot json\n".to_vec());
        let one: Option<Result<i64, String>> =
            exchange(&mut sent, &mut answers, &"first", "Worker").unwrap();
        assert_eq!(one, Some(Ok(7)));
        assert_eq!(sent, b"\"first\"\n");
        let two: Option<Result<i64, String>> =
            exchange(&mut sent, &mut answers, &"second", "Worker").unwrap();
        assert_eq!(two, Some(Err("no".into())));
        let bad = exchange::<_, Result<i64, String>>(&mut sent, &mut answers, &"third", "Worker");
        assert!(bad.unwrap_err().starts_with("Worker response: "));
        // nothing left to read: no answer, not an error
        let gone: Option<Result<i64, String>> =
            exchange(&mut sent, &mut answers, &"fourth", "Worker").unwrap();
        assert_eq!(gone, None);
    }
}
