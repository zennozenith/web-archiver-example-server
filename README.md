# Web Archiver Example Server

A self-hosted **web archive server** — save pages (HTML + screenshot + metadata) with version history, a pending queue, and a tagging system. Comes with a built-in web UI and a browser-extension-compatible API.

Two fully functional, API-compatible implementations are included:

| Implementation | Stack | Entry point |
| -------------- | ----- | ----------- |
| **Rust** | tokio + axum + sqlx (SQLite) + tracing | [`rust/src/main.rs`](rust/src/main.rs) |
| **TypeScript** | Bun + `bun:sqlite` (zero npm dependencies) | [`typescript/server.ts`](typescript/server.ts) |

The companion **browser extension** is available at [zennozenith/web-archiver-browser-extention](https://github.com/zennozenith/web-archiver-browser-extention).

## Features

- 🗂️ **Archives with version history** — every capture of a URL is stored as a separate version (`page.html`, `screenshot.png/jpg`, `metadata.json`)
- 🏷️ **Tags** — create, rename, and delete tags; tags are shared across all versions of a URL
- 📥 **Pending queue** — queue URLs to archive later; the queue drains automatically when the URL is archived
- 🖥️ **Web UI** — `index.html` is served at `/`
- 🔌 **Browser-extension friendly** — CORS enabled everywhere, plus a `/api/pending/complete` endpoint for extension workflows — see [zennozenith/web-archiver-browser-extention](https://github.com/zennozenith/web-archiver-browser-extention)
- 🔒 **Safe serving** — archived pages are served with a `sandbox` Content-Security-Policy and `X-Content-Type-Options: nosniff`
- ♻️ **Auto-import** — existing archive folders on disk that aren't in the DB yet get indexed on startup (TypeScript server)

## Getting started

### Rust

Uses [devenv](https://devenv.sh) in this repo (Rust nightly, mold, OpenSSL). Or run manually with a stable toolchain (edition 2024 required):

```sh
cd rust
cargo run
```

### TypeScript

Requires [Bun](https://bun.sh) — no `bun install` needed (zero dependencies):

```sh
cd typescript
bun run server.ts        # or: bun dev (watch mode)
```

### Configuration

| Env var | Default | Description |
| ------- | ------- | ----------- |
| `PORT` / `APP_SERVER__PORT` | `3000` / `8888` (Rust, via devenv) | Server port |
| `HOST` | `127.0.0.1` | Bind address |
| `DATA_DIR` | script dir / `.` | Where the SQLite DB and `archives/` live |
| `MAX_BODY_MB` | `256` | Max request body size (MB) |
| `RUST_LOG` | `info` | Rust tracing filter |

## API

```
GET    /                         Web UI (index.html)
POST   /api/archive              multipart/form-data: html, screenshot, screenshot-format ("png"|"jpeg"),
                                 metadata (+ optional url, title, tags)
GET    /api/check?url=           Archive status + version history (alias: /api/status)
GET    /api/archives             All archived URLs, grouped, with versions
DELETE /api/archives?url=        Delete every version of a URL
DELETE /api/archives/:id         Delete a single version
GET    /api/tags                 All tags with usage counts
POST   /api/tags                 { name } create a tag
PATCH  /api/tags/:id             { name } rename a tag
DELETE /api/tags/:id             Delete a tag (from every site)
PUT    /api/sites/tags           { url, tags: [...] } replace a website's tags (creates unknown ones)
GET    /api/pending              Pending queue -> { items, total }
POST   /api/pending              { url, title?, tags? } add to the queue
PATCH  /api/pending/:id          { tags: [...] } replace the queued tags
DELETE /api/pending/:id          Remove from the queue
POST   /api/pending/complete     { id?, url? } mark done (extension compatible)
GET    /api/health               Stats
GET    /archives/:folder/:file   page.html | screenshot.png | screenshot.jpg | metadata.json
```

### Example

```sh
curl -X POST http://localhost:3000/api/archive \
  -F "url=https://example.com" \
  -F "html=<h1>Example</h1>" \
  -F 'metadata={"title":"Example"}' \
  -F "tags=docs,reference"
```

## Development

The repo ships a [`devenv.nix`](devenv.nix) with the Rust toolchain, `bacon`, `cargo-nextest`, and common Cargo utilities. Inside the devenv shell:

```sh
devenv shell
devenv tasks run app:test   # cargo test
```

## License

Licensed under either of

- **Apache-2.0** — see [LICENSE-APACHE-2.0](LICENSE-APACHE-2.0)
- **MIT** — see [LICENSE-MIT](LICENSE-MIT)

at your option. Files may indicate their specific license or copyright status; unless stated otherwise, the dual license above applies.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
