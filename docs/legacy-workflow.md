# Command and workflow reference

Detailed search, tagging, OCR and embedding examples. See [installation](installation.md)
and [concurrency](concurrency.md) for component layout, folder scope, paths and write protocol.

Rust meme tagging, OCR, and a searchable thumbnail window. Run `memetag` for the command list.

## Search operators

`AND`, `OR`, and `NOT` are case-insensitive, standalone operators outside quotes.
`NOT` binds most tightly, then `AND`, then `OR`. Symbolic forms remain supported:

```text
folder:reactions AND NOT format:gif
text:cat OR text:dog
(text:cat OR text:dog) AND format:png
```

Multiword tags such as `this is fine` remain single terms. Quote literal phrases
containing operator words or punctuation: `text:"this AND that, OR something"`.
`text:OR` searches for the literal text `OR`; `"AND"` searches for a tag named `and`.
Unclosed quotes and missing operator operands report an error.

`t:` is a short alias for `text:` everywhere, e.g. `t:group project` or
`t:cat OR t:dog`.

`text:` matches anywhere in the OCR text, so `text:tea` also finds `instead`.
`text.word:tea` matches only whole words: `tea.` and `TEA` count, `instead` and
`team` do not. Words are runs of letters and digits, so punctuation and line
breaks between them are ignored and `text.word:"green tea"` finds `green,\ntea`.
Wildcards apply per word: `text.word:te*` finds `tea` and `team` but not
`instead`. `t.w:` is the short alias, e.g. `t.w:tea AND NOT t.w:coffee`.

The **?** button at the right of the search bar opens a syntax card listing every
operator and field with examples, including the whole-word cases above. **Escape**
closes the card before it closes the window.

The search bar suggests tags used in the collection (including derived tags),
remembered tag entries, and configured aliases. Supplemental tags that have
never been used locally stay in the tag editors. **Up/Down** select a suggestion;
**Tab** or a click completes the term at the cursor and updates results without
copying. Surrounding commas, AND/OR/NOT, negation, and parentheses are retained.
Suggestions overlay the grid without moving its tiles. **Escape** dismisses
suggestions first; another Escape closes the window. **Enter** still copies the
first result and stays open. Accepted suggestions are remembered locally.

Completions are quoted literal tags, so names containing commas, parentheses,
Boolean words, wildcard characters, quotes, or reserved field names work safely.
For example, `"width:1920"` matches that literal tag, whereas `width:1920` checks
image width. Quotes around an entire term now mean exact tag matching; use
`text:"some words"` for OCR phrases. Within quotes, `\"` and `\\` represent a
literal quote and backslash. Other backslashes remain literal. Tag completion
does not replace field values such as `t:...` or `width.gt:...`.

## Image search and duplicates

`similar:<image>` finds the files that look like an image: a re-encode, another
size, a captioned or lightly cropped copy. The value is a path in the library
(`similar:"Old/cat (1).jpg"`, quoted when it holds parentheses or commas), a
file on this machine (`similar:~/Downloads/meme.png`), or `clipboard` for the
image on the clipboard. Results come closest first, so Enter copies the best
match. It combines with everything else: `similar:~/x.png AND NOT folder:old`.

In the grid, the **camera button** beside OCR searches for the clipboard image,
and the **Similar** button that appears beside **Edit** on a hovered tile
searches for that tile. **Duplicates** on the select bar groups the current
search results by look-alikes and shows each group as a row of tiles: click a
tile to select it, **Select group** takes the row, and **Edit tags** then works
on the selection; **Show in grid** turns a group into a `similar:` search. The
card follows the search, so narrowing the search narrows the groups. memetag
deletes nothing; what to do with a duplicate is decided elsewhere.

**Propose** on the select bar takes the selection as the definition of a tag
they share and ranks every file without that tag by likeness, closest first.
Likeness is one of three, remembered per tag: **image** (a CLIP ViT-B/32 vector
of the thumbnail: characters, art style, templates), **text** (TF-IDF over the
OCR text: tags that live in the caption) or **both**. Left-click a tile to
accept it (green), again to reject it (red), again to clear; **Accept** hands
the green ones to the batch panel as a reviewed `+tag` plan, and the red ones
are remembered against the tag so the next ranking pulls away from them (at
half weight, measured). Middle-click copies a tile as the grid does, right-click
opens the original, and **Edit** on a hovered tile opens the editor; a file
that comes back carrying the tag leaves the list. The tag's earlier rejections
can be shown and un-rejected from the card. Nothing is tagged without that
review. `memetag embeddings` computes the
vectors once (about 20 ms an image on the CPU, from the thumbnails); it needs
the model file `~/.local/share/memetag/models/clip-vit-b32-vision.onnx`
(Xenova/clip-vit-base-patch32, `onnx/vision_model.onnx`, 352 MB) or
`embed_model` in config.toml. `memetag propose <tag> --measure` hides a third
of a tag's files and reports where they land, per mode, which is how to tell
whether a tag is one the ranking can see.

The match is a perceptual hash (256 bits, pHash with a median threshold) of the
image scaled to fit 256×256; two files match within 20 differing bits.
`similar:~/x.png@30` loosens that. Real copies sit at 2 bits (median) and
within 12 at the 95th percentile; unrelated images never come under 100. A
group holds files that all match each other, so look-alikes never chain two
files that do not resemble each other. Flat images (solid colours, blank pages)
match nothing, since their hash carries no detail.

```text
memetag similar ~/Downloads/meme.png             # closest files, with their distance
memetag similar clipboard --grab                 # the clipboard image, in the grid
memetag dupes                                    # every look-alike group in the library
```

Hashes live in the index and are made from the local thumbnail cache, so the
first use hashes the whole library in seconds without touching the file server;
`pull` keeps them complete afterwards. A file whose pixels change is hashed
again; a file that only gets tags is not.

## Inspect before copying

**PageUp/PageDown** scroll the thumbnail grid by a screenful; **Home/End** jump
to the first/last row. These work while the search box has focus. Modified keys
(such as Shift+Home/End for text selection) keep their text-editing behavior.
Keyboard jumps ease over about a quarter second, even across large collections.

Right-click a thumbnail to open the original fullscreen in an external viewer:

- **feh** for still images, including still PNG and WebP. `--scale-down` keeps
  oversized images within the screen.
- **mpv** for videos, all GIFs, animated PNG (APNG), and animated WebP, with
  `--loop-playlist=inf --loop-file=no` and `--autofit-larger=100%x100%`.

The single-file playlist loop reloads the original each pass, avoiding replay
errors observed with `--loop-file=inf` on APNG/WebP in the installed mpv.

Animation is detected from PNG/WebP container metadata, not filename extensions.
Detection and launching happen off the UI thread. The grid stays open and the
clipboard is unchanged; close the viewer with its normal controls (q in feh/mpv).
Closing memetag does not deliberately close an already opened viewer.
Viewer launch/exit errors appear in memetag.

**Left-click copies and closes** after success; **middle-click copies and keeps
the grid open**. Copy errors keep it open. Enter copies the first result and
stays open. Clipboard behavior is independent of the external viewer: PNG and
GIF retain their original bytes, JPEG/WebP become PNG images at original
dimensions (animated WebP becomes a still), and videos copy a file link.
A GIF is offered both as `image/gif` and as its file link, because Qt
applications such as Telegram Desktop and 64gram take a file link before image
data and would otherwise paste the first frame as a still PNG; browsers and
Discord keep taking the `image/gif` bytes. memetag owns the X11 clipboard
itself (`memetag _clip-owner`, a detached process that lives until something
else copies, as xclip does), so several formats can be offered at once.
The receiving application determines support for GIFs and file links.

The previous built-in zoom/pan preview is preserved in Git on
`archive/builtin-preview` at `be12364`. It is no longer part of the active UI.

## Edit tags and OCR

Hover a thumbnail and click **Edit**, or **Shift+left-click** it. Add/remove tags
and correct OCR in the multiline text box. Autocomplete puts your existing tags,
previous saved entries, and aliases first. Folder and implied tags appear in a
separate read-only list. **Save** / **Ctrl+S** updates the file and search results;
**Cancel** / **Escape** returns to the grid, with a discard prompt for unsaved edits.
In the tag field, **Up/Down** choose a highlighted suggestion and **Tab** adds it.
Focus stays in the field for the next tag. **Enter** adds exactly what you typed;
with no suggestions, Tab moves focus normally. Suggestions also accept clicks.

Editing OCR marks it human-reviewed, including deliberately empty text. This
overrides machine results even from an older OCR worker already running. Future
OCR and embedding passes respect the embedded review marker. Uncheck the review
box to return to machine text. Saves preserve modification time at nanosecond
precision, inode/birth time, and verified pixels. MKV/WebM editing is not supported.

`memetag refresh-vocabulary` downloads a bounded seed of up to 1,000 popular
supplemental tag names into a local cache. Suggestions work offline; typing never
sends searches to a service. Suggested tags are only applied when you choose them.

For isolated collections/tests, `MEMETAG_ROOT`, `MEMETAG_DB`, and `MEMETAG_THUMBS`
override the collection, index, and thumbnail cache paths respectively.

## Select and batch-tag

**Shift+middle-click** toggles selection without copying. Selected tiles keep a
blue border and checkmark. **Select** mode also lets ordinary left-click toggle
selection; Shift+left-click still opens the individual editor and plain
middle-click still copies. The toolbar shows the selection count, including
selections hidden by the current search. **Clear** deselects everything and is
disabled when the selection is empty. **Select all N search results** is explicit.

**Edit tags** opens separate Add/Remove lists with keyboard autocomplete. Indexed
counts show which embedded tags occur on the selection. Review the lists and
file count, then **Apply to N images**. This merges the requested changes into
each file's current tags; unrelated tags and OCR remain intact. Existing matching
files are skipped without a write. Saves preserve modification dates and verify
media. Progress reports per-file failures; Stop finishes the current file first.
Done refreshes search and deselects successes, retaining failures/unprocessed
files for retry. Implied tags can change when their source tags change.

**Tagging assistant** (right end of the select bar) shows how much of the
collection carries tags you wrote yourself: `Tagged by hand: N of M files (x%)`,
the same for the current search results, and a link that adds `tag_count:0` to
the search to show only the untagged ones. While it is on, untagged tiles get an
orange outline and tagged ones are dimmed; hovering an untagged tile says so.
Folder and implied tags do not count, only tags embedded by you or a batch edit.

For the desktop's shared collection, `[batch_remote]` in `config.toml` maps
`local_root` to server `root`, with `host` and a configured `command` that runs
the same-version `memetag _batch-worker` under `nice -n 19 ionice -c 3` on the file server.
Requests and returned metadata use JSON over SSH; media stay on the server.
The helper neither runs OCR nor touches the server's index. Local fixture
collections execute locally. Individual tag/editor/OCR metadata writes use this
same helper, sending the XMP packet and full-file revisions; they reject stale
reads before writing. Network mounts without a matching remote refuse writes.
The mount type distinguishes network-mounted collections from native filesystems;
path spelling alone does not determine routing. The remote helper is
installed separately from the server's existing memetag executable. Update it
with the clients; older helpers reject the new single-file request safely.

Vocabulary sync compares the server revision under a server-side lock before
replacing rules. A concurrent change asks you to retry instead of overwriting it.
Local rule saves also reject stale drafts. See [concurrency and deployment](concurrency.md)
for guarantees, regression checks, and upgrading an existing installation.

## Embed previously saved OCR

```text
memetag embed-text [--db PATH] [--root DIR] [--limit N] [--dry-run]
```

This command reads nonempty saved text from the SQLite index and embeds it as
`meme:text`. It never runs OCR or loads a model. The source database is opened
read-only and left unchanged; `--db` can point to a transferred SQLite backup.
`--root` specifies the collection containing the relative paths in that index.
Both default to the usual memetag configuration.

Files already containing identical text or human-reviewed text are skipped without a write. Changed
text is written through the existing verified in-place writer: modification time
is restored at nanosecond precision, the inode is retained (preserving birth time),
and decoded pixels are verified. Metadata writes change ctime, as Linux requires.
Empty OCR results are skipped, and existing tags are retained using the normal
tag vocabulary rules.

Errors are reported per file and cause a nonzero exit. Rerunning retries failed
files and skips successful matches. SIGINT/SIGTERM finish the current file and
stop; rerunning resumes by checking what is actually in each file. `--limit N`
bounds attempted writes to files with differing text; matching files do not use
the limit. `--dry-run` reports proposed writes without modifying files.

`ocr --embed` still embeds text as it is newly inferred; use `embed-text` to embed
completed results or retry failed embedding without paying for another OCR pass.

### File-server embedding workflow

Run bulk embedding on the file server where the original files live, after a
filesystem snapshot or backup. Do not run a bulk pass over a client's network mount.

1. Make a consistent backup of the desktop OCR index with SQLite's `.backup`
   command. Do not copy just the live `.sqlite` file while WAL writes are active.
2. Transfer that backup to the file server. Build memetag in a local clone there, never on
   the sshfs path. Pass the transferred backup with `--db` and the server's actual
   collection directory with `--root`.
3. Preview with `--dry-run`, then run `nice -n 19 ionice -c 3 memetag embed-text ...`
   using those same paths. This embedding step requires no Ollama service.
4. A backup captures only results completed at backup time. Transfer a newer
   backup and rerun to embed additional results. Reindex on the server later to
   refresh other metadata changed by the normal tag writer, such as stored IDs.

The desktop OCR service can continue throughout development and backup creation;
the embedding command consumes its own short database snapshot and does not write
completion records or replace the source index.

## Tests and fuzzing

Run `cargo test --locked`. The property tests use fixed seeds and bounded inputs,
with no new test dependencies. They check wildcard matching against an independent
DP matcher, Unicode query quoting and completion ranges, XMP replacement and
foreign-property preservation (including deliberately empty manual OCR), cyclic
implications against graph reachability, and complete-linkage duplicate groups.
Container tests replace and remove metadata in generated JPEG/PNG/GIF/WebP/BMP,
checked-in APNG/animated WebP, synthetic MP4/AVIF boxes, and a fallback payload.
The MP4/AVIF fixtures test container metadata only, not video or AV1 decoding.

The CLI smoke test runs one second of mutation fuzzing on an isolated generated
corpus. It exercises container parsing, XMP, search, and journaled writes on
scratch copies. For a longer campaign:

```sh
MEMETAG_TEST_FUZZ_SECONDS=60 timeout 120s cargo test --locked --test fuzz_cli -- --nocapture
```

Failures retain the temporary corpus and print its path. Successful runs clean
it up. This is seeded mutation/property fuzzing, not coverage-guided libFuzzer;
it does not establish that arbitrary input sizes or recursion depths are safe.
The one-second slow-input detector reports only after a call returns, so keep
an external timeout for longer runs. Use a debug build to catch arithmetic overflow.

For an existing **scratch corpus** (never the live collection):

```sh
cargo build --locked
MEMETAG_JOURNAL=/tmp/memetag-fuzz-journal timeout 75s target/debug/memetag _fuzz /path/to/scratch-corpus 60 12345
target/debug/memetag _replay /path/to/scratch-corpus/_fuzz_crashers/query-seed12345-iter42-panic.bin
```

Keep the saved filename: `_replay` selects query, XMP, writer, or container checks
from its prefix. Writer replay creates its own temporary file and journal. A seed
and unchanged corpus give the same mutation sequence (files are sorted); iteration
counts vary with machine speed. The fuzzer saves panics and slow inputs under
`_fuzz_crashers` and exits unsuccessfully when it finds either.
