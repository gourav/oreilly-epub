# Copilot Instructions

## Build, Test, and Lint

```bash
cargo build                  # debug build
cargo build --release        # release binary → target/release/oreilly-epub

cargo test --all             # run all tests
cargo test <test_name>       # run a single test

cargo fmt --all              # format (CI uses --check)
cargo clippy --all-targets --all-features -- -D warnings  # lint (warnings = errors in CI)
```

CI runs `fmt --check` then `clippy` then `test` in sequence; all must pass before a PR can merge.

## Architecture

The tool is a single Rust binary (`src/main.rs`) that downloads an O'Reilly book by numeric ID and packages it as a valid EPUB. The pipeline is:

1. **CLI + lock** (`main.rs`) — Parses args (clap derive), acquires `queue.lock` so only one download runs at a time. Additional invocations enqueue into `queue.txt` and exit; the primary process drains the queue automatically with a random 10–15 s delay between books.

2. **Auth** (`http_client.rs`) — Reads a flat `cookies.json` (key → value) and builds a `reqwest::Client` with a `Jar` scoped to `learning.oreilly.com`.

3. **API fetching** (`main.rs`) — Two paginated O'Reilly REST endpoints are consumed:
   - `/api/v2/epubs/urn:orm:book:{id}/` → `EpubResponse` (metadata, URLs for chapters and files)
   - chapters URL → `Vec<Chapter>` (title, stylesheet URLs per chapter)
   - files URL → `Vec<FileEntry>` (url, `full_path` relative path, media type)

4. **Parallel download** (`epub.rs: download_all_files`) — Files are fetched concurrently up to `--parallel` (default 3, max 8) using `FuturesUnordered`. Streams are piped directly to disk via `tokio::io::copy`.

5. **EPUB assembly** (`epub.rs: create_epub_archive`) — Writes a ZIP with EPUB-spec constraints:
   - `mimetype` must be first and **uncompressed** (`CompressionMethod::Stored`).
   - `META-INF/container.xml` is generated via `ogrim::xml!` macro pointing at the OPF.
   - Chapter XHTML files are rewritten by `xml.rs:build_epub_chapter`.
   - The OPF is patched by `xml.rs:write_modified_opf` to inject `dc:description`.
   - Non-chapter XHTML files are sanitized by `xml.rs:sanitize_xhtml_file`.
   - All other files are copied verbatim.

6. **XML processing** (`xml.rs`) — Uses `quick_xml` for event-driven parse/rewrite and `ogrim::xml!` for generating boilerplate XML. Key transformations:
   - Absolute O'Reilly API URLs in `src`/`href` attributes → relative paths inside the EPUB.
   - HTML5 void elements (`<br>`, `<img>`, etc.) converted to self-closing XML form.
   - HTML5 boolean attributes (e.g. `async`) normalized to `attr="attr"` for XML validity.
   - `<script>` elements stripped entirely.
   - Source document `<html>/<head>/<body>` structure replaced by a clean EPUB XHTML wrapper.

## File Layout

| File | Responsibility |
|------|---------------|
| `src/main.rs` | CLI, lock/queue, orchestration |
| `src/http_client.rs` | Cookie-authenticated `reqwest::Client` |
| `src/epub.rs` | Parallel download + ZIP/EPUB creation |
| `src/xml.rs` | XHTML/OPF rewriting (quick_xml + ogrim) |
| `src/models.rs` | `serde` models for O'Reilly API responses |

## Runtime Paths

- **Download cache**: `{XDG_DATA_HOME}/oreilly-epub/files/{bookid}/` (mirrors `full_path` hierarchy)
- **Output EPUB**: `~/Downloads/oreilly-epub/{bookid}.epub`
- **Config/cookies**: searched in order: `--cookies <path>`, `{XDG_CONFIG_HOME}/oreilly-epub/cookies.json`, `./cookies.json`
- **Lock + queue**: `{XDG_DATA_HOME}/oreilly-epub/queue.lock` / `queue.txt`

## Key Conventions

- **Copyright header**: Every `src/*.rs` file begins with a two-line GPLv3 copyright comment. Add it to new files.
- **XML generation**: Use `ogrim::xml!` macro for producing XML/XHTML markup. Do not use string formatting for XML.
- **XML parsing/rewriting**: Use `quick_xml` event-driven API (`Reader` + `Writer`). Process events in a `loop { match reader.read_event_into(&mut buffer) { … } buffer.clear(); }` pattern.
- **Relative paths**: Use `relative_path::RelativePathBuf` (not `std::path`) for all paths stored inside the EPUB to stay cross-platform.
- **Error handling**: Use `anyhow::Result` with `.context("…")` to wrap errors at call sites. Propagate with `?`.
- **`ourn` field**: The `ourn` string on `FileEntry` and `Chapter` is the O'Reilly URN used as a shared key to match chapters to their file entries during EPUB assembly.
- **`--embed-styles` flag**: When set, CSS file contents are inlined as `<style>` blocks in each chapter `<head>` instead of `<link>` elements.
- **Branch naming**: `feat/<description>`, `fix/<description>`, `docs/<description>`.
- **Commit messages**: Imperative subject line; reference issues with `Fixes #N`.
