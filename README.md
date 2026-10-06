# memetag

[![CI](https://github.com/nyanmeister/memetag/actions/workflows/ci.yml/badge.svg)](https://github.com/nyanmeister/memetag/actions/workflows/ci.yml)

memetag keeps a meme collection searchable. Tags, and the text in the picture,
are written **into the files themselves** as XMP, so they survive copying,
renaming, and moving the collection to another machine. A local SQLite index
makes search instant; it is only a cache and can be rebuilt at any time.

The everyday loop: type a few words, click the picture, paste it into the chat.

### Search by tag
<img width="1092" height="752" alt="Example usage with the hand-made tag 'amogus'." src="https://github.com/user-attachments/assets/79bf6d67-9a60-4a1d-be7b-477e87201c0b" />

### Search by the text in the picture (OCR)
<img width="1092" height="752" alt="Example usage of searching the text 'always has been'." src="https://github.com/user-attachments/assets/45582940-9517-4c22-884d-6585d0362a6c" />

## What it does

- **Tags live in the files.** Booru-style tags (`cat`, `artist:example`,
  `this is fine`) are stored as XMP in JPEG, PNG, GIF, WebP, AVIF and MP4 files.
  A write leaves the pixels, the modification time and the inode untouched, is
  verified afterwards, and is journaled so an interrupted write is restored on
  the next start. Other XMP already in the file is preserved.
- **The caption is searchable.** OCR (Tesseract, or a vision model through
  Ollama) reads the text; `text:` and whole-word `text.word:` search it. Text
  you correct by hand is saved in the file and marked as reviewed, so no later
  OCR pass overwrites it.
- **A search bar that speaks booru.** `cat, -dog || text:*hello*`, or
  `AND`/`OR`/`NOT` in words. Wildcards, namespaces, size, format, date, folder
  and source filters, with tag completion as you type. The **?** button beside
  the bar lists the whole grammar.
- **A grab window.** Thumbnails, newest first. Left-click copies the picture to
  the clipboard and closes the window; middle-click copies and stays; right-click
  opens the original in `feh` or `mpv`. Videos play as storyboards in the grid.
- **Look-alikes.** `similar:` finds re-encodes, resizes and captioned copies of
  an image, and **Duplicates** groups a whole result set by resemblance. memetag
  deletes nothing; what to do with a duplicate stays your call.
- **Tag suggestions you review.** **Propose** ranks the files that lack a tag by
  likeness to the files that carry it, by image (CLIP vectors), by caption text,
  or both. You accept or reject each one; nothing is tagged without that review.
- **Batch editing.** Select tiles, then add and remove tags across all of them in
  one pass.
- **Several folders, local or on a file server.** Register folders anywhere as
  named sources. A folder on an SSH-reachable server is scanned and written on
  that server, while the index stays on your machine.
- **A phone grid.** `memetag serve` shows the same grid in a browser: a tap
  copies the picture, **Share** opens the share sheet. It runs in Termux on
  Android.
- **A plain CLI** for all of the above, for scripts and for machines without a
  display.

## Platform

Linux. The desktop window draws with OpenGL on X11 or Wayland; copying to the
clipboard needs X11 or XWayland. The CLI and the web grid also build in Termux
on Android, see [the phone section](docs/installation.md#phone-termux).

## Install

You need stable Rust and a C compiler. Desktop builds also need the OpenGL,
X11/Wayland and xkbcommon development libraries; the
[build notes](docs/installation.md#build-components) list the packages for
Ubuntu and Arch. Cargo fetches the Rust dependencies, and SQLite is compiled in.

Build what you need:

```sh
cargo build --locked --release -p memetag                 # CLI only
cargo build --locked --release -p memetag -p memetag-gui  # CLI and desktop window
cargo build --locked --release --workspace                # also the CLIP helper
```

Put the resulting executables (`memetag`, `memetag-gui`, and optionally
`memetag-infer`) together in a directory on your PATH. The CLI finds the others
beside itself or on PATH, so an unpacked archive works too.

memetag calls a few other programs for specific jobs. None is needed for
tagging, indexing or searching:

| Program | Used for |
|---|---|
| `feh` | Opening original still images from the grid |
| `mpv` | Opening original videos, GIFs and animated PNG/WebP |
| FFmpeg (`ffmpeg`, `ffprobe`) | Video previews and AVIF decoding |
| Tesseract, or an Ollama server with a vision model | Reading captions. Saved text stays searchable without either |

The CLIP model for **Propose** is a separate download; see
[image search](docs/legacy-workflow.md#image-search-and-duplicates).

## First run

```sh
memetag init /path/to/your/memes   # writes config.toml; refuses to replace one
memetag reindex                    # reads the tags already in the files
memetag thumbs                     # builds the thumbnail cache
memetag grab                       # opens the window
```

`memetag doctor` reports where everything lives and which optional programs it
found. `memetag folders menu` (or the **Folders** button in the window) chooses
which subfolders are indexed; the whole collection is in until you say
otherwise.

## Everyday use

In the window: **left-click** copies and closes, **middle-click** copies and
stays, **right-click** opens the original, **Enter** copies the first result.
Hover a tile and click **Edit**, or **Shift+left-click**, to change its tags and
caption; **Ctrl+S** saves. **Shift+middle-click** selects tiles for a batch
edit or for **Propose**; **Duplicates** groups whatever the current search shows.

The same from the command line:

```sh
memetag tag picture.png '+cat' '+artist:example' '-wip'
memetag read picture.png
memetag search 'cat, -dog || text:*hello*'
memetag search 'folder:reactions AND NOT format:gif'
memetag search 'width.gt:1000, created_at.gt:2019'
memetag similar ~/Downloads/meme.png
memetag dupes
memetag ocr                        # reads captions the chosen engine has not done yet; safe to stop and resume
memetag untagged
memetag tags
```

The [command reference](docs/legacy-workflow.md) has the full search grammar,
the look-alike search, batch tagging and OCR embedding in detail.

## More than one folder

Each source is a named folder with its own subfolder selection. Add a local
folder, or one on a file server that runs the memetag CLI; search spans every
enabled source, and a disabled source keeps its cached tags, text and vectors
until you turn it back on.

```sh
memetag sources add Downloads /absolute/path/to/downloads
memetag sources add-network Reactions /mnt/share/Reactions --host user@server --server-root /srv/Reactions
memetag pull                       # refresh the index from the servers
memetag search 'source:Downloads, cat'
```

Network sources are scanned and written on their server over SSH, under `nice`
and `ionice`, so the heavy work stays where the files are. Details in
[multiple sources](docs/installation.md#multiple-sources) and
[concurrent writes](docs/concurrency.md).

## Learn more

- [Installation](docs/installation.md): build components, optional programs,
  file servers and the phone.
- [Command and workflow reference](docs/legacy-workflow.md): search operators,
  image search, editing, batch tagging, OCR embedding and tests.
- [Concurrent writes and remote helpers](docs/concurrency.md), and
  [network backend plans](docs/network-sources.md).
- [Release preparation](docs/releasing.md); `tools/package.sh` builds local
  archives and `packaging/PKGBUILD` prepares Arch split packages.
- Every executable answers `--version` without configuration, a display or a
  model.

License: GPL-3.0-or-later.
