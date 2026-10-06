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
