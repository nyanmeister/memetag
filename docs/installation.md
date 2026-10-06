# Installation

Memetag runs on Linux. The browser needs OpenGL and X11 or Wayland. Clipboard
ownership currently requires X11/XWayland. Tags are embedded in the media; the
SQLite index and thumbnails stay local. OCR and image inference are optional.

## Build components

Install stable Rust and a C compiler/linker. Desktop builds also need OpenGL,
X11/Wayland and xkbcommon libraries at runtime; install your distribution's
development packages and `pkg-config` where required. Building the optional
inference helper needs OpenSSL development libraries for its native TLS setup.
The [CI workflow](../.github/workflows/ci.yml) lists the packages used for Ubuntu
builds; [packaging/PKGBUILD](../packaging/PKGBUILD) lists Arch package dependencies.

Rust dependencies are declared in the workspace's `Cargo.toml` files and pinned
by `Cargo.lock`; Cargo downloads them when building. SQLite is compiled into the
program, so a separate SQLite server or installation is not required.

Choose the components you need:

```sh
cargo build --locked --release -p memetag                 # CLI / server helpers
cargo build --locked --release -p memetag -p memetag-gui  # normal desktop
cargo build --locked --release --workspace               # also image inference
```

The core build excludes eframe/egui and ONNX. The inference package is the only
package depending on `ort`; its build can download upstream ONNX runtime binaries.
`Cargo.lock` is shared by the workspace. The vision model is a separate download
and is never bundled in a release archive. All executables answer `--version`
without configuration, a display, or a model.

Install the selected executables together in a directory on PATH. The browser
starts with `memetag grab`. `memetag folders menu` opens the folder menu directly.
Helpers resolve beside the executable, then on PATH, so they also work in an
unpacked archive. Server `_pull-worker` and `_batch-worker` use only the CLI.

Run `tools/package.sh desktop /where/to/packages` to create a version-checked
archive without installing it. Packaging also needs jq and ripgrep. Modes `core` and `full` select the other component
sets. `CARGO_TARGET_DIR` is respected. The archive includes runtime-library and
checksum manifests; it is a native build for the build host, not a static binary
or a promise of compatibility with older distributions.

The local Arch split-package recipe is `packaging/PKGBUILD`. From a clean commit,
create its input beside a copy of the recipe:

```sh
git archive --prefix=memetag-0.3.1/ HEAD | gzip > /package-work/memetag-0.3.1.tar.gz
cp packaging/PKGBUILD /package-work/
cd /package-work
makepkg -s
```

No package enables a service or changes the user's configuration automatically.
The example source checksum is `SKIP` for a locally produced archive; a published
source recipe must replace it with the verified archive checksum.

## First collection

```sh
memetag init /absolute/path/to/collection
memetag folders menu
memetag reindex
memetag thumbs
memetag grab
```

`init` refuses to replace existing configuration. `doctor` reports paths and
optional dependencies. Existing installations with `root` in config.toml keep
working; the default selection remains the whole root until changed.

## Multiple sources

**Folders** manages sources and the subfolders within each. Give an outside local
folder a name and full path, then use **Add folder** and **Save sources**. Run
`memetag pull` or reopen the browser to discover its files. Files stay in place.
Sources can be renamed or disabled. **Locate folder** confirms a moved source's
new location, preserving its cached tags, OCR, vectors and thumbnail identities.

```sh
memetag sources add Downloads /absolute/path/to/downloads
memetag sources status                     # shows permanent IDs and availability
memetag folders --source SOURCE_ID exclude private
memetag sources disable SOURCE_ID
memetag sources locate SOURCE_ID /new/location
memetag search 'source:Downloads, cat'
memetag search 'source:"Reaction images"'
```

Sources are stored in `sources.toml`, beside config.toml. IDs are permanent and
independent of names and paths. The initial source is `main`, named Memes;
existing `library.toml` selections and matching pull/batch server settings are
imported automatically. After import, edit selections through the menu or CLI.
Subfolder rules are relative to each source: the most specific rule wins, with
exclusion winning a tie. `.` includes the source and files directly inside it.
No includes, or disabling the source, hides its files without deleting cache.
Overlapping mounted roots, overlapping server roots on the same configured SSH
host, and aliases of the same local directory are rejected. Automatic filesystem
watching is deferred.

To add a network folder, check **Network source** in Folders. It starts with the
existing server's SSH host and helper commands; **Reuse server** switches the
template. Enter a name and the full mounted path on this computer. Leave
**Folder on server** blank when the server uses the same full path, or enter its
actual server path. **Add folder**, then **Save sources**, then Refresh/pull.
Existing network sources expose **Server settings** for their host, server path
and helper commands. **Locate folder** changes only the mounted path.

```sh
# Reuse Memes' host and helper commands, with a different source folder.
memetag sources add-network Reactions /mnt/pictures/Reactions
# Different mount spelling on this computer; reuse another source's server.
memetag sources add-network Archive /mnt/archive --server SOURCE_ID --server-root /srv/images/archive
# Another SSH server; install the CLI there first, or specify its helper paths.
memetag sources add-network Remote /mnt/remote --host user@other-server --server-root /srv/images
```

The default template is `main`. With `--host`, the default helper commands are
`nice -n 19 ionice -c 3 memetag _pull-worker` and the corresponding `_batch-worker`.
Use `--pull-command` and `--batch-command` to supply installed absolute helper
paths when `memetag` is absent from the server's noninteractive PATH. SSH uses
your existing keys, agent and SSH configuration. Registration and network Locate
work while disconnected and do not inspect the mount; refresh checks connectivity.
Original viewing/copying still needs the mounted files. Each network source must
have an SSH-capable file server; see [backend plans](network-sources.md) for shares
without SSH. Different host aliases or server symlinks can conceal duplicate
roots, so use a consistent host spelling and distinct physical server folders.

Selection applies to search, export, duplicates, OCR, inference, reindex and
pulls. Offline sources remain searchable from cache. Local directory identity
checks detect replaced folders; use Locate to confirm intentional replacement.
An empty or unreadable source listing retains cached rows. Cache size does not
shrink when sources are disabled. Each source refreshes independently so a
server failure does not prevent local folders from being updated. Menus refuse
stale saves and edits refuse a changed source configuration; reopen the window.

The first index opening migrates schema 2 to 3 in a transaction and creates a
coherent `index.before-sources-*.sqlite` backup for a populated index. Close old
browsers and stop old background jobs before upgrading from 0.2. Versions before 0.3
can reset a schema 3 index: restore the pre-migration index and configuration
before reverting binaries. Do not run older binaries against the migrated DB.
Migration and initial config import are startup operations; a first
`pull --dry-run` may perform them, although the pull merge itself is read-only.

Explicit `read`, `tag`, and `clip` remain file-oriented. `rescan` accepts absolute
paths, `SOURCE_ID/relative/path`, or a path relative to the main source.
Snapshot `embed-text --db --root` still accepts older indexes; for schema 3,
`--root` addresses `main` by default, or `--source SOURCE_ID` selects another
source. Without `--root`, schema 3 uses the current registry and enabled scope.

XDG_CONFIG_HOME, XDG_DATA_HOME and XDG_CACHE_HOME are honored when absolute;
otherwise ~/.config, ~/.local/share and ~/.cache are used. Existing MEMETAG_ROOT,
MEMETAG_DB, MEMETAG_THUMBS and MEMETAG_JOURNAL overrides remain available. Once
sources.toml exists, MEMETAG_ROOT must agree with its main source; use Locate to
move it or a separate XDG_CONFIG_HOME to select another library. Moving
an existing installation to different XDG directories requires moving its config,
index and cache deliberately. A malformed folder configuration fails closed.

## Optional programs

Install these separately for the corresponding features. External viewers and
local command-line tools must be available on PATH; Cargo does not install them.

| Dependency | Used for |
|---|---|
| `feh` | Opening original still images from the desktop browser |
| `mpv` | Opening original videos, GIFs, animated PNG and animated WebP |
| FFmpeg (`ffmpeg` and `ffprobe`) | Video information, previews and AVIF decoding |
| `ffmpegthumbnailer` | Optional alternative video thumbnail generator |
| Tesseract with language data | Local OCR; the current command uses English (`eng`) data |
| Ollama server with a vision model | Alternative OCR engine; configure its URL and model |
| OpenSSH client; SSH access and the CLI on the file server | Network-source scans and metadata writes |
| Fontconfig (`fc-match`) and an installed CJK font | International caption fallback |
| systemd user units | Optional resumable OCR and periodic pulls |

`feh` and `mpv` are required for their respective **Open original** actions;
tagging, indexing and searching do not require them. Existing embedded or indexed
OCR remains searchable without Tesseract or Ollama. Speech/translation commands
and their models are configured separately. Run `memetag doctor` to report paths
and detect available optional programs.

## File servers

Keep the index local. Initial matching `[pull_remote]` and `[batch_remote]`
sections in config.toml are imported into `main` when sources.toml is first created;
afterwards the source registry is authoritative. Configure additional servers in
Folders or with `sources add-network`, as described above. Run bulk
scans and writes on the file server. `reindex` refuses a network-mounted root;
`pull` requires its configured server. New workers prune excluded branches;
clients also filter old-worker listings. Wire request defaults retain compatibility
with previous callers. Update the CLI helpers and reopen desktop browsers when
updating the implementation. Never remove write journals as part of an upgrade.

## Phone (Termux)

The CLI and browser grid run in Termux on Android. The desktop GUI is not required;
original-file access needs local files or a configured SSH source. For mobile data,
use an SSH host alias reachable outside your LAN, with its port and trusted host key
configured in Termux. A LAN-only server address will not work without a route to it.

- `pkg install rust clang git`, clone, `cargo build --release -p memetag`, symlink the binary into `$PREFIX/bin`. No source change is needed.
- The collection is the server's, mounted with sshfs inside Termux (a FUSE mount there
  needs root). Only Termux and its children see that mount; other apps, including
  Termux:API's share sheet, do not.
- Do not `reindex` or `thumbs` through the mount. Copy the desktop's `index.sqlite`
  and `thumbs/` once; keep `sources.toml` ids identical to the desktop's so the copy
  stays coherent; `memetag pull` keeps it fresh from the server afterwards.
- `memetag serve` is the phone's grid: open `http://127.0.0.1:7777/` in the browser.
  Tapping a picture copies it to the clipboard; the small **Share** button on each tile
  opens the share sheet; ⤢ opens the original. config.toml there:

```toml
# mount_command = "/path/to/your/mount-script"  # optional command to mount the collection
# share_command defaults to `termux-share -a send {file}` when termux-share is on PATH,
# share_stage to true (copy the pick under the cache first, where the share sheet can read it).
# Both only matter for the no-script fallback (see below); the browser's own share sheet needs neither.
```

- Start it from `~/.termux/boot/` (Termux:Boot) to run after a reboot. After updating
  the source, rebuild with `cargo build --locked --release -p memetag` and restart
  the running server. Updating the binary does not replace a running process.
  If process-name tools fail on Android, stop the server by its PID.
- **Both tile actions need a full Chromium browser** (Cromite, Brave, Chrome). Copy writes a PNG
  with the async Clipboard API: Firefox-family browsers cannot (GeckoView's Android clipboard
  carries text and HTML only, so the write fails with "Clipboard write is not allowed"); the
  system WebView and browsers built on it refuse with "Write permission denied". Share uses the
  Web Share API with the file, which Firefox for Android does not support for files. Measured
  2026-10-04 with Iceraven 2.48, Jelly on WebView 93, and Cromite 153 (works).
  Chromium's "Install and create shortcut" menu item uses the page's manifest for a home-screen icon.
- **Unreliable connections:** Copy starts its clipboard operation during your tap
  and waits for the complete download. Share may show **Share now** when a slow
  transfer finishes; tap it to open the share sheet without downloading again.
  Older browsers can similarly show **Copy now**. Transfer controls show progress,
  let you cancel, retry temporary connection failures up to twice, and offer
  **Retry** if the file stays unavailable. Open also waits for a complete original.
  Configured SSH sources fetch originals directly from their server, so viewing
  does not depend on a healthy phone-side network mount. Cached thumbnails still
  work without that connection; unavailable originals still need the server.
- **Clipboard permissions:** memetag writes images but never reads your clipboard.
  A delayed clipboard call can cause a permission prompt after the tap expires;
  the immediate Copy operation avoids that in current Chromium browsers. If the
  browser does ask, its site permission controls the grant; the webapp cannot make
  it permanent itself. Use the same origin as your shortcut: `localhost:7777` and
  `127.0.0.1:7777` have separate site settings. A clipboard grant does not replace
  the fresh tap required to open a share sheet.
- **Why Share is the browser's call and not `termux-share`:** Android 10+ aborts an activity
  started by an app that is not in front ("Abort background activity starts" in logcat), and a
  share sheet opened by the server process is exactly that while the browser is in front. It only
  ever worked with Termux itself in the foreground (how it was first tested, 2026-10-04). The
  server-side `/share` route stays for browsers without script or `navigator.share`, and for a
  desktop (`memetag clip`).
