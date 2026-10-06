# Portability

Memetag supports Linux local collections and optional SSH-backed network sources.
The CLI also builds in Termux on Android; the phone browser interface uses
`memetag serve`. See [installation](docs/installation.md) for requirements and setup.

- `memetag init` creates configuration for a chosen collection; `doctor` reports
  paths and available optional programs.
- Absolute XDG_CONFIG_HOME, XDG_DATA_HOME and XDG_CACHE_HOME are honored. Runtime
  configuration, models, media, databases, caches and SSH credentials are not bundled.
- Core and desktop builds do not depend on ONNX Runtime. Image inference is a
  separate helper and needs a separately installed CLIP model.
- CPU workers default to a quarter of available logical CPUs, at least one.
  Thumbnail textures default to a 256 MiB budget. Override `index_threads` and
  `texture_budget_mb` in config.toml, or MEMETAG_TEX_MB for texture memory.
- Native filesystem reads and writes work locally. Linux mount metadata identifies
  network filesystems; scans and writes use configured SSH workers on their server.
  Any SSH-capable Linux file server can be used; no particular mount path is required.
- Desktop rendering uses OpenGL on X11 or Wayland; clipboard selection ownership
  needs X11/XWayland. Phone copy and share actions depend on browser APIs.
- Optional external viewers, OCR, speech and translation programs are configurable.
  Clients can read embedded OCR without installing an inference engine.
- Archives are native builds and list their runtime library dependencies. Test on
  the intended distribution; an archive built on one Linux release is not guaranteed
  to work on older releases.

memetag's own XMP properties live under the namespace `https://github.com/nyanmeister/memetag/ns/meme/1.0/`.
It is an identifier, not a service endpoint: memetag never contacts it. Every program
writing a collection must use the same namespace, so keep the default unless your files
already carry another; `xmp_namespace` in config.toml changes what new writes use, and
`legacy_xmp_namespaces` lists namespaces earlier builds wrote, which are read as memetag's
own and rewritten under the current one the next time each file is written. `memetag doctor`
prints both.
