# Contributing to oreilly-epub

First off — thank you for your interest in contributing! \
This document explains how to set up your environment, run checks/tests, make
changes, and submit pull requests (PRs).

> **License note:** By contributing, you agree that your contributions will be
> licensed under the project's license (GPLv3). See `LICENSE.txt` in the repo.

## Project goals

This tool downloads an O'Reilly (Safari) book by ID and assembles a valid EPUB
by fetching metadata/chapters/assets, cleaning up XHTML, injecting styles, and
writing a standards-compliant archive. The pipeline includes:

- Authenticated HTTP client using a user-provided `cookies.json`.
- Parallel file downloads with configurable concurrency (default 4, max 8).
- XHTML/OPF processing to ensure EPUB validity (e.g., void tags, stylesheet
links, `dc:description` injection).
- CI/CD pipelines that run `rustfmt`, `clippy`, tests, and build release
artifacts on tag pushes.

## Getting started

### Prerequisites

- **Rust toolchain** (stable) with `cargo`. The CI uses
`dtolnay/rust-toolchain@stable`, so match stable locally.
- For actual end-to-end runs: a **`cookies.json`** file (flat key-value JSON of
cookies for `https://learning.oreilly.com`). Place it in one of:
  1. `--cookies <path>` argument
  2. `${XDG_CONFIG_HOME}/oreilly-epub/cookies.json`
  3. the project's current directory (`./cookies.json`) \
     The app will search these locations in this order.

> **Note:** The app is asynchronous (Tokio) and uses `reqwest`. No special
> services are required to build and run locally.

### Repository layout (high-level)

- `src/main.rs` – CLI, argument parsing, orchestration (fetch metadata, pages,
downloads, build EPUB).
- `src/http_client.rs` – authenticated client from `cookies.json`.
- `src/epub.rs` – download pipeline and zip/EPUB creation (mimetype first,
`META-INF/container.xml`, etc.).
- `src/xml.rs` – XHTML/OPF rewriting (void tags, attribute rewrite, stylesheet
linking, description injection).
- `src/models.rs` – data models for APIs (EPUB, chapters, files, pagination).

### Quick start

```bash
# 1) Clone
git clone <REPO_URL>
cd oreilly-epub

# 2) (Optional) Place cookies.json (see above)
# Example:
# cp /path/to/cookies.json .

# 3) Build
cargo build

# 4) Check format & lints locally
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings

# 5) Run unit tests
cargo test --all

# 6) Try it (example book id)
cargo run -- 9781492097039 --parallel 4
# Add --skip-download if you already have cached files
# Add --cookies ./cookies.json to point to a specific file
```

## Development workflow

### Style and linting

- **Formatting:** `cargo fmt --all` (CI runs `cargo fmt --all -- --check`).
- **Linting:** `cargo clippy --all-targets --all-features -- -D warnings` (CI
treats warnings as errors).

> PRs must pass both format and clippy checks locally before you push.

## Before you open a PR

### Commit messages

- Use clear, imperative subject lines: “Fix EPUB container.xml path
resolution”, not “fixed stuff”.
- Wrap body lines at \~72 characters when practical.
- Reference issues with `Fixes #123` / `Closes #123` when appropriate.

### Branch naming

Use short, descriptive branches, e.g.:

- `feat/parallelism-8-limit`
- `fix/xhtml-void-tags`
- `docs/contributing`

### Checklists

Before pushing:

- [ ] `cargo fmt --all` produces no diffs.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes locally.
- [ ] `cargo test --all` is green.
- [ ] If you touched XML/OPF/XHTML handling, tested at least one real book
locally (if possible).
- [ ] Updated documentation, comments, or usage text when applicable.
