# memetag

Memetag stores booru-style tags inside image and video files using XMP. Search, browse thumbnails, edit tags and OCR text, find duplicates, and review suggested tags. The local SQLite index speeds up browsing; tags stay with copied files.



Memetag is intended for **Linux**. The desktop browser uses OpenGL with X11 or
Wayland; clipboard copying requires X11/XWayland. The CLI and web grid also work
in Android's Termux; see [installation](docs/installation.md#phone-termux).

The Rust workspace separates three executables:

| Component | Purpose |
|---|---|
| `memetag` | CLI, local indexing/search, safe metadata writes, OCR and file-server workers |
| `memetag-gui` | Optional browser, editor and library-folder menu |
| `memetag-infer` | Optional CLIP image-vector generation, with ONNX isolated here |

Dependencies:

- **Building:** stable Rust and a C compiler/linker; desktop builds also need
  the graphics/input libraries listed in [installation](docs/installation.md#build-components).
  Cargo installs the Rust dependencies from `Cargo.toml` and `Cargo.lock`.
- **Opening originals:** install `feh` for still images and `mpv` for videos,
  GIFs, animated PNG and animated WebP. These programs must be on PATH.
- **Video previews and AVIF:** install FFmpeg (including `ffprobe`).
- **Optional OCR:** install Tesseract with language data, or configure an Ollama
  server with a suitable vision model. Reading saved OCR needs neither engine.

```sh
cargo build --locked --release -p memetag -p memetag-gui
memetag init /path/to/collection
memetag folders menu
memetag reindex
memetag thumbs
memetag grab
```

The browser's **Folders** button manages named sources and their included subfolders. Add outside local or network folders without moving the files; search spans all enabled sources. Disable a source or subfolder to hide its cached rows while preserving OCR and vectors. Rename or locate a moved source without changing its identity. Network sources each have SSH settings; reuse an existing server or configure another. Scans and writes run on that server while the index stays local.

```sh
memetag tag picture.png '+cat' '+artist:example'
memetag read picture.png
memetag search 'cat, -dog || text:*hello*'
memetag folders exclude 'Old/private'
memetag folders include 'Old/private/cats'
memetag sources add Downloads /absolute/path/to/downloads
memetag sources add-network Reactions /mounted/share/Reactions --server main
memetag pull --no-thumbs
memetag search 'source:Downloads, cat'
memetag doctor
memetag --help
memetag --version
```

The browser copies and closes itself on left-click, copies and stays open on middle-click, and opens originals on right-click. Edit on hover or Shift+click opens the editor; Ctrl+S saves. See the [command reference](docs/legacy-workflow.md) for detailed examples. Boolean search supports tags, namespaces, wildcard tags, paths/folders, dimensions, formats, dates, OCR text, and visual similarity. Metadata writes preserve media payloads and timestamps, verify their result, and journal interrupted writes.

## EXAMPLES:

### Search by tag:
<img width="1092" height="752" alt="Example usage with the hand-made tag 'amogus'." src="https://github.com/user-attachments/assets/79bf6d67-9a60-4a1d-be7b-477e87201c0b" />

### OCR (search by visible text):
<img width="1092" height="752" alt="Example usage of searching the text 'always has been'." src="https://github.com/user-attachments/assets/45582940-9517-4c22-884d-6585d0362a6c" />


See [installation](docs/installation.md), [concurrent writes and remote helpers](docs/concurrency.md),
[network backend plans](docs/network-sources.md), and [release preparation](docs/releasing.md). `tools/package.sh` creates local
archives; `packaging/PKGBUILD` prepares Arch split packages for local builds.
License: GPL-3.0-or-later.
