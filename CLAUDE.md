# CLAUDE.md

## Project

mdserve is a markdown preview server built as a companion for AI coding agents.
See the [README](README.md) for project overview and the
[architecture doc](docs/architecture.md) for design details.

## Build and test

```bash
cargo build --release
cargo test                            # all tests
```

Rust 1.82+, 2021 edition. Templates are embedded at compile time via
minijinja-embed (changes to `templates/` require a rebuild).

## Project structure

- `src/main.rs` - CLI parsing and entry point
- `src/app.rs` - Axum router, handlers, path resolver, markdown rendering,
  lazy state, SSE live reload, file watcher
- `templates/` - MiniJinja templates (Jinja2 syntax), embedded at compile time

Tests live inline in `src/app.rs` (`#[cfg(test)] mod tests`); run them with
plain `cargo test`.

## Design constraints

- **Agent-companion scope.** mdserve renders markdown that AI agents produce
  during coding sessions. Features that push it toward a documentation platform,
  configurable server, or deployment target are out of scope.
- **Zero config.** `mdserve file.md` must work with no flags or config files.
- **Base-dir boundary.** `--base-dir` (default: cwd) is a security fence. Any
  file under it is browsable; nothing above it is ever served. This lets one
  document link sideways to a sibling directory.
- **Lazy render, permanent cache.** Nothing is scanned, rendered, or watched at
  startup. A file is rendered on first request and cached forever; it is watched
  only while open in a browser.
- **Minimal client-side JS.** Most logic is server-side. Client JS handles
  theme selection and SSE reload only.

## Changelog

Generated with [git-cliff](https://git-cliff.org/) using `cliff.toml`. To
update `CHANGELOG.md`:

```bash
git cliff -o CHANGELOG.md
```

## Commits

Use conventional commits: `type: lowercase description` (e.g. `feat:`, `fix:`,
`chore:`, `docs:`, `refactor:`, `test:`). No scopes, no emojis. Subject line
max 72 chars, imperative mood. Body optional, wrap at 72 chars, explain why not
what.
