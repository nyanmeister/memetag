//! X11 clipboard owner that serves several targets at once. xclip serves exactly one, and a GIF offered
//! only as `image/gif` reaches Qt apps (Telegram Desktop, 64gram) as a still first frame: they take a
//! decoded image before anything else unless the clipboard also carries file URLs, which they check first.
//! `memetag _clip-owner [CLIPBOARD|PRIMARY]` reads a bundle from stdin, takes the selection, prints `ok`, and stays
//! alive in its own session until another program takes it, exactly as xclip does. Big payloads go by INCR.
//! `read_selection` is the other direction, for middle-click paste.
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::*;
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

pub struct Entry {
    pub mime: String,
    pub data: Vec<u8>,
}

/// Hand `entries` to a detached owner process for CLIPBOARD; returns once it holds the selection.
pub fn own(entries: &[Entry]) -> Result<(), String> {
    own_on("CLIPBOARD", entries)
}
/// The same for any selection (`CLIPBOARD` or `PRIMARY`).
pub fn own_on(selection: &str, entries: &[Entry]) -> Result<(), String> {
    let exe = crate::self_exe().ok_or("clipboard owner: this program's own path is unknown")?;
    let mut cmd = Command::new(exe);
    cmd.arg("_clip-owner")
        .arg(selection)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Own session: the owner must outlive the grid window and whatever terminal launched it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("clipboard owner: {e}"))?;
    write_bundle(&mut child.stdin.take().unwrap(), entries)
        .map_err(|e| format!("clipboard owner: {e}"))?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        let _ = child.wait();
    }); // reap it whenever it ends; the grid may live for hours
    match line.trim() {
        "ok" => Ok(()),
        "" => Err("clipboard owner exited early".into()),
        e => Err(e.to_string()),
    }
}

/// Text for other programs, served by the same detached owner so it outlives the grid window (the toolkit's own
/// clipboard dies with the process: text copied just before a copy-and-close or an Escape was gone by the time it
/// was pasted). UTF8_STRING is what X11 programs ask for first; STRING is Latin-1 by contract, so it is offered only for ASCII.
pub fn own_text(selection: &str, text: &str) -> Result<(), String> {
    let bytes = text.as_bytes();
    let mut entries: Vec<Entry> = ["UTF8_STRING", "text/plain;charset=utf-8", "text/plain"]
        .iter()
        .map(|m| Entry {
            mime: m.to_string(),
            data: bytes.to_vec(),
        })
        .collect();
    if text.is_ascii() {
        entries.push(Entry {
            mime: "STRING".into(),
            data: bytes.to_vec(),
        });
    }
    own_on(selection, &entries)
}

/// The text another program holds on `selection` (`PRIMARY` for a middle-click paste), as UTF8_STRING.
pub fn read_selection(selection: &str) -> Result<String, String> {
    read_target(selection, "UTF8_STRING").map(|d| String::from_utf8_lossy(&d).into_owned())
}
/// The image on the clipboard as PNG bytes (browsers, image viewers and memetag's own owner all offer image/png).
pub fn read_image() -> Result<Vec<u8>, String> {
    read_target("CLIPBOARD", "image/png")
}
/// One target of a selection, as the owner sends it; INCR transfers are followed. Errors when nothing is selected,
/// the owner has no such target, or it does not answer within a second.
fn read_target(selection: &str, target: &str) -> Result<Vec<u8>, String> {
    let e = |e: &dyn std::fmt::Display| e.to_string();
    let (conn, win) = selection_window().map_err(|x| e(&x))?;
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .map_err(|x| e(&x))?
            .reply()
            .map(|r| r.atom)
            .map_err(|x| e(&x))
    };
    let (sel, want, prop, incr) = (
        atom(selection)?,
        atom(target)?,
        atom("MEMETAG_PASTE")?,
        atom("INCR")?,
    );
    conn.convert_selection(win, sel, want, prop, x11rb::CURRENT_TIME)
        .map_err(|x| e(&x))?;
    conn.flush().map_err(|x| e(&x))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    // wait for one event, bounded: the owner may be a program that never answers
    let next = |want: &mut dyn FnMut(&Event) -> bool| -> Result<(), String> {
        loop {
            match conn.poll_for_event().map_err(|x| e(&x))? {
                Some(ev) if want(&ev) => return Ok(()),
                Some(_) => {}
                None => {
                    if std::time::Instant::now() > deadline {
                        return Err("the selection owner did not answer".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    };
    let mut answered = None;
    next(&mut |ev| match ev {
        Event::SelectionNotify(n) if n.requestor == win => {
            answered = Some(n.property);
            true
        }
        _ => false,
    })?;
    if answered == Some(x11rb::NONE) || answered.is_none() {
        return Err(if target == "UTF8_STRING" {
            "nothing selected".to_string()
        } else {
            format!("the owner offers no {target}")
        });
    }
    let fetch = || {
        conn.get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX)
            .map_err(|x| e(&x))?
            .reply()
            .map_err(|x| e(&x))
    };
    let first = fetch()?;
    let mut data = first.value;
    if first.type_ == incr {
        data = vec![];
        loop {
            // each chunk arrives as a new value of the property; an empty one ends the transfer
            next(
                &mut |ev| matches!(ev, Event::PropertyNotify(p) if p.window == win && p.atom == prop && p.state == Property::NEW_VALUE),
            )?;
            let chunk = fetch()?;
            if chunk.value.is_empty() {
                break;
            }
            data.extend(chunk.value);
        }
    }
    Ok(data)
}

fn write_bundle(w: &mut impl Write, entries: &[Entry]) -> std::io::Result<()> {
    for e in entries {
        write!(w, "{}\n{}\n", e.mime, e.data.len())?;
        w.write_all(&e.data)?;
    }
    w.flush()
}
fn read_bundle(r: &mut impl BufRead) -> std::io::Result<Vec<Entry>> {
    let mut out = vec![];
    loop {
        let mut mime = String::new();
        if r.read_line(&mut mime)? == 0 {
            return Ok(out);
        }
        let mut len = String::new();
        r.read_line(&mut len)?;
        let len: usize = len
            .trim()
            .parse()
            .map_err(|_| std::io::Error::other("bad bundle"))?;
        let mut data = vec![0; len];
        r.read_exact(&mut data)?;
        out.push(Entry {
            mime: mime.trim().to_string(),
            data,
        });
    }
}

/// The `_clip-owner` process.
pub fn serve(selection: &str) -> Result<(), String> {
    let entries = read_bundle(&mut std::io::stdin().lock()).map_err(|e| e.to_string())?;
    if entries.is_empty() {
        return Err("nothing to own".into());
    }
    let r = run(entries, selection);
    // Never reach the parent's closed pipe with a second line; the first line is the whole protocol.
    r.map_err(|e| e.to_string())
}

/// A 1×1 window that is never mapped, to own a selection or ask for one with; it listens for property changes
/// because the server timestamp and INCR chunks both arrive as PropertyNotify on it.
fn selection_window(
) -> Result<(x11rb::rust_connection::RustConnection, Window), Box<dyn std::error::Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let (root, visual) = {
        let screen = &conn.setup().roots[screen_num];
        (screen.root, screen.root_visual)
    };
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    Ok((conn, win))
}

fn run(entries: Vec<Entry>, selection: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (conn, win) = selection_window()?;
    let atom = |name: &str| -> Result<Atom, Box<dyn std::error::Error>> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let (clipboard, targets, timestamp, incr) = (
        atom(selection)?,
        atom("TARGETS")?,
        atom("TIMESTAMP")?,
        atom("INCR")?,
    );
    let mimes = entries
        .iter()
        .map(|e| atom(&e.mime))
        .collect::<Result<Vec<_>, _>>()?;
    // A real server timestamp (ICCCM): touch a property on our window and read the time off the notify.
    conn.change_property8(PropMode::APPEND, win, targets, AtomEnum::STRING, &[])?;
    conn.flush()?;
    let time = loop {
        if let Event::PropertyNotify(e) = conn.wait_for_event()? {
            if e.window == win {
                break e.time;
            }
        }
    };
    conn.set_selection_owner(win, clipboard, time)?;
    if conn.get_selection_owner(clipboard)?.reply()?.owner != win {
        return Err("could not take the clipboard".into());
    }
    println!("ok");
    let _ = std::io::stdout().flush();
    let chunk = (conn.maximum_request_bytes() / 4).clamp(4096, 1 << 20);
    let mut transfers: Vec<(Window, Atom, usize, usize)> = vec![]; // (requestor, property, entry, next offset)
    loop {
        match conn.wait_for_event()? {
            Event::SelectionClear(e) if e.selection == clipboard => return Ok(()),
            Event::SelectionRequest(r) => {
                let property = if r.property == x11rb::NONE {
                    r.target
                } else {
                    r.property
                };
                let ok = if r.target == targets {
                    let mut list = vec![targets, timestamp];
                    list.extend(&mimes);
                    conn.change_property32(
                        PropMode::REPLACE,
                        r.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok()
                } else if r.target == timestamp {
                    conn.change_property32(
                        PropMode::REPLACE,
                        r.requestor,
                        property,
                        AtomEnum::INTEGER,
                        &[time],
                    )
                    .is_ok()
                } else if let Some(i) = mimes.iter().position(|&m| m == r.target) {
                    let data = &entries[i].data;
                    if data.len() <= chunk {
                        conn.change_property8(
                            PropMode::REPLACE,
                            r.requestor,
                            property,
                            r.target,
                            data,
                        )
                        .is_ok()
                    } else {
                        // INCR: announce the size, then feed a chunk each time the requestor deletes the property.
                        conn.change_window_attributes(
                            r.requestor,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )?;
                        transfers.push((r.requestor, property, i, 0));
                        conn.change_property32(
                            PropMode::REPLACE,
                            r.requestor,
                            property,
                            incr,
                            &[data.len() as u32],
                        )
                        .is_ok()
                    }
                } else {
                    false
                };
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: r.time,
                    requestor: r.requestor,
                    selection: r.selection,
                    target: r.target,
                    property: if ok { property } else { x11rb::NONE },
                };
                conn.send_event(false, r.requestor, EventMask::NO_EVENT, notify)?;
                conn.flush()?;
            }
            Event::PropertyNotify(e) if e.state == Property::DELETE => {
                if let Some(k) = transfers
                    .iter()
                    .position(|t| t.0 == e.window && t.1 == e.atom)
                {
                    let (w, p, i, off) = transfers[k];
                    let data = &entries[i].data;
                    let end = (off + chunk).min(data.len());
                    conn.change_property8(PropMode::REPLACE, w, p, mimes[i], &data[off..end])?;
                    conn.flush()?;
                    if off == data.len() {
                        transfers.remove(k); // the empty write above ends the transfer
                        if !transfers.iter().any(|t| t.0 == w) {
                            conn.change_window_attributes(
                                w,
                                &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
                            )?;
                            conn.flush()?;
                        }
                    } else {
                        transfers[k].3 = end;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bundle_round_trips() {
        let entries = vec![
            Entry {
                mime: "image/gif".into(),
                data: b"GIF89a\n\n\0\xff".to_vec(),
            },
            Entry {
                mime: "text/uri-list".into(),
                data: b"file:///a b.gif\r\n".to_vec(),
            },
            Entry {
                mime: "x/empty".into(),
                data: vec![],
            },
        ];
        let mut buf = vec![];
        write_bundle(&mut buf, &entries).unwrap();
        let back = read_bundle(&mut &buf[..]).unwrap();
        assert_eq!(back.len(), 3);
        for (a, b) in entries.iter().zip(&back) {
            assert_eq!(a.mime, b.mime);
            assert_eq!(a.data, b.data);
        }
        assert!(read_bundle(&mut &b"image/gif\nnope\n"[..]).is_err());
    }
}
