//! Web archiver: self-hosted archive server (tokio + axum + sqlx/sqlite + tracing).
//!
//!   GET    /                         Web UI (index.html in DATA_DIR)
//!   POST   /api/archive              multipart/form-data: html, screenshot, screenshot-format ("png"|"jpeg"), metadata (+ optional url, title, tags)
//!   GET    /api/check?url=           Archive status + version history (alias: /api/status)
//!   GET    /api/archives             All archived URLs, grouped, with versions
//!   DELETE /api/archives?url=        Delete every version of a URL
//!   DELETE /api/archives/:id         Delete a single version
//!   GET    /api/tags                 All tags with usage counts
//!   POST   /api/tags                 { name } create a tag
//!   PATCH  /api/tags/:id             { name } rename a tag
//!   DELETE /api/tags/:id             Delete a tag (from every site)
//!   PUT    /api/sites/tags           { url, tags: [...] } replace a website's tags (creates unknown ones)
//!   GET    /api/pending              Pending queue -> { items, total }
//!   POST   /api/pending              { url, title?, tags? } add to the queue (tags queued for the URL)
//!   PATCH  /api/pending/:id          { tags: [...] } replace the queued tags (also PUT)
//!   DELETE /api/pending/:id          Remove from the queue
//!   POST   /api/pending/complete     { id?, url? }
//!   GET    /api/health               Stats
//!   GET    /archives/:folder/:file   page.html | screenshot.png | screenshot.jpg | metadata.json
//!
//! Env: PORT (3000), HOST (127.0.0.1), DATA_DIR (.), MAX_BODY_MB (256), RUST_LOG (info)

use std::{
    collections::{HashMap, HashSet},
    path::{Component, Path as FsPath, PathBuf},
    str::FromStr,
    sync::Arc,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Multipart, Path, Query, Request, State, multipart::MultipartRejection,
    },
    http::{HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{
    FromRow, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

// ---------- State / errors ----------

#[derive(Clone)]
struct AppState {
    db: SqlitePool,
    data_dir: Arc<PathBuf>,
    archives_dir: Arc<PathBuf>,
}

struct AppError(StatusCode, String);

impl AppError {
    fn bad(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        error!("database error: {e}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        error!("io error: {e}");
        Self(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

type ApiResult<T = Json<Value>> = Result<T, AppError>;

// ---------- Models ----------

#[derive(Debug, Clone, FromRow)]
struct ArchiveRow {
    id: String,
    url: String,
    title: String,
    folder: String,
    archived_at: String,
    html_size: i64,
    has_screenshot: i64,
    screenshot_ext: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
struct PendingRow {
    id: String,
    url: String,
    title: String,
    added_at: String,
    tags: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Version {
    id: String,
    archived_at: String,
    url: String,
    folder: String,
    title: String,
    html_size: i64,
    has_screenshot: bool,
    screenshot_ext: Option<String>,
}

impl From<&ArchiveRow> for Version {
    fn from(r: &ArchiveRow) -> Self {
        Self {
            id: r.id.clone(),
            archived_at: r.archived_at.clone(),
            url: r.url.clone(),
            folder: r.folder.clone(),
            title: r.title.clone(),
            html_size: r.html_size,
            has_screenshot: r.has_screenshot != 0,
            screenshot_ext: r.screenshot_ext.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PendingItem {
    id: String,
    url: String,
    title: String,
    added_at: String,
    tags: Vec<String>,
}

impl From<PendingRow> for PendingItem {
    fn from(r: PendingRow) -> Self {
        Self {
            id: r.id,
            url: r.url,
            title: r.title,
            added_at: r.added_at,
            tags: r
                .tags
                .map_or_else(Vec::new, |t| parse_tags(Value::String(t))),
        }
    }
}

#[derive(Deserialize)]
struct UrlQuery {
    url: Option<String>,
}

// ---------- Helpers ----------

fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn normalize_url(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw.trim()).ok()?;
    matches!(u.scheme(), "http" | "https").then(|| u.to_string())
}

/// Only plain single-segment-style relative names are allowed (no `..`, no absolute paths).
fn is_safe_rel(name: &str) -> bool {
    !name.is_empty()
        && FsPath::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

async fn remove_folder(archives_dir: &FsPath, folder: &str) {
    if !is_safe_rel(folder) {
        warn!("refusing to remove unsafe folder name {folder:?}");
        return;
    }
    if let Err(e) = tokio::fs::remove_dir_all(archives_dir.join(folder)).await
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!("failed to remove {folder}: {e}");
    }
}

async fn remove_versions(st: &AppState, rows: &[ArchiveRow]) -> Result<usize, AppError> {
    for r in rows {
        remove_folder(&st.archives_dir, &r.folder).await;
        sqlx::query("DELETE FROM archives WHERE id = ?")
            .bind(&r.id)
            .execute(&st.db)
            .await?;
    }
    Ok(rows.len())
}

async fn versions_for_url(db: &SqlitePool, url: &str) -> Result<Vec<ArchiveRow>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM archives WHERE url = ? ORDER BY archived_at DESC")
        .bind(url)
        .fetch_all(db)
        .await
}

// ---------- tags ----------

#[derive(Debug, FromRow)]
struct TagRef {
    id: String,
    name: String,
}

/// Tags attached to a URL, sorted by name (shared by every version of the URL).
async fn tags_for_url(db: &SqlitePool, url: &str) -> Result<Vec<TagRef>, sqlx::Error> {
    sqlx::query_as(
        "SELECT st.tag_id AS id, t.name AS name FROM site_tags st
         JOIN tags t ON t.id = st.tag_id
         WHERE st.url = ? ORDER BY t.name COLLATE NOCASE",
    )
    .bind(url)
    .fetch_all(db)
    .await
}

fn tags_json(tags: Vec<TagRef>) -> Vec<Value> {
    tags.iter()
        .map(|t| json!({ "id": t.id, "name": t.name }))
        .collect()
}

/// Trim a tag name to at most 64 characters.
fn clean_tag_name(raw: &str) -> String {
    raw.trim().chars().take(64).collect()
}

/// Parse a tags value: JSON array, comma/semicolon-separated string, or array.
fn parse_tags(v: Value) -> Vec<String> {
    match v {
        Value::Array(items) => items.into_iter().flat_map(parse_tags).collect(),
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                vec![]
            } else if s.starts_with('[') {
                serde_json::from_str::<Value>(s)
                    .ok()
                    .map(parse_tags)
                    .unwrap_or_default()
            } else {
                s.split([',', ';']).map(str::to_owned).collect()
            }
        }
        _ => vec![],
    }
}

/// Find or create tags by name (case-insensitive); returns their ids in the given order.
/// Runs on the given connection, so callers can keep it inside their own transaction.
async fn ensure_tag_ids(
    ex: &mut sqlx::SqliteConnection,
    names: Vec<String>,
) -> Result<Vec<String>, sqlx::Error> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw in names {
        let name = clean_tag_name(&raw);
        if name.is_empty() || !seen.insert(name.to_lowercase()) {
            continue;
        }
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM tags WHERE name = ? COLLATE NOCASE")
                .bind(&name)
                .fetch_optional(&mut *ex)
                .await?;
        match existing {
            Some(id) => ids.push(id),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                sqlx::query("INSERT INTO tags (id, name) VALUES (?, ?)")
                    .bind(&id)
                    .bind(&name)
                    .execute(&mut *ex)
                    .await?;
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// Replace the tag set attached to a URL; unknown tag names are created.
/// Returns the new tag list as `{ id, name }` JSON values.
async fn set_site_tags(
    db: &SqlitePool,
    url: &str,
    names: Vec<String>,
) -> Result<Vec<Value>, sqlx::Error> {
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM site_tags WHERE url = ?")
        .bind(url)
        .execute(&mut *tx)
        .await?;
    let ids = ensure_tag_ids(&mut tx, names).await?;
    for id in &ids {
        sqlx::query("INSERT OR IGNORE INTO site_tags (url, tag_id) VALUES (?, ?)")
            .bind(url)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    let tags = tags_for_url(db, url).await?;
    Ok(tags_json(tags))
}

// ---------- DB init / import ----------

async fn init_db(path: &FsPath) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    for stmt in [
        "CREATE TABLE IF NOT EXISTS archives (
            id             TEXT PRIMARY KEY,
            url            TEXT NOT NULL,
            title          TEXT NOT NULL,
            folder         TEXT NOT NULL UNIQUE,
            archived_at    TEXT NOT NULL,
            html_size      INTEGER NOT NULL DEFAULT 0,
            has_screenshot INTEGER NOT NULL DEFAULT 0,
            screenshot_ext TEXT,
            metadata       TEXT
        )",
        "CREATE INDEX IF NOT EXISTS idx_archives_url ON archives (url, archived_at DESC)",
        "CREATE TABLE IF NOT EXISTS tags (
            id   TEXT PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE
        )",
        // Site tags are shared across all versions of a URL (tags are not per-version).
        "CREATE TABLE IF NOT EXISTS site_tags (
            url    TEXT NOT NULL,
            tag_id TEXT NOT NULL,
            PRIMARY KEY (url, tag_id)
        )",
        "CREATE INDEX IF NOT EXISTS idx_site_tags_tag ON site_tags (tag_id)",
        "CREATE TABLE IF NOT EXISTS pending (
            id       TEXT PRIMARY KEY,
            url      TEXT NOT NULL UNIQUE,
            title    TEXT NOT NULL DEFAULT '',
            added_at TEXT NOT NULL,
            tags     TEXT
        )",
    ] {
        sqlx::query(stmt).execute(&pool).await?;
    }
    Ok(pool)
}

/// Index archive folders already on disk that aren't in the DB yet.
async fn import_existing_folders(st: &AppState) -> Result<(), AppError> {
    let known: HashSet<String> = sqlx::query_scalar("SELECT folder FROM archives")
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .collect();

    let mut imported = 0usize;
    let mut rd = tokio::fs::read_dir(&*st.archives_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let folder = entry.file_name().to_string_lossy().into_owned();
        if known.contains(&folder) {
            continue;
        }
        let dir = entry.path();
        let Ok(raw) = tokio::fs::read(dir.join("metadata.json")).await else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<Value>(&raw) else {
            continue;
        };
        let Some(url) = meta
            .get("url")
            .and_then(Value::as_str)
            .and_then(normalize_url)
        else {
            continue;
        };

        let size = |name: &'static str| {
            let p = dir.join(name);
            async move {
                tokio::fs::metadata(p)
                    .await
                    .map(|m| m.len() as i64)
                    .unwrap_or(0)
            }
        };
        let html_size = size("page.html").await;
        let shot_ext = if size("screenshot.png").await > 0 {
            Some("png")
        } else if size("screenshot.jpg").await > 0 {
            Some("jpg")
        } else {
            None
        };

        let archived_at = match meta
            .get("capturedAt")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(s) => s.to_owned(),
            None => tokio::fs::metadata(&dir)
                .await
                .and_then(|m| m.modified())
                .map(|t| iso(DateTime::<Utc>::from(t)))
                .unwrap_or_else(|_| iso(Utc::now())),
        };
        let title = meta
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(&url)
            .to_owned();

        let res = sqlx::query(
            "INSERT INTO archives (id, url, title, folder, archived_at, html_size, has_screenshot, screenshot_ext, metadata)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&url)
        .bind(title)
        .bind(&folder)
        .bind(archived_at)
        .bind(html_size)
        .bind(shot_ext.is_some() as i64)
        .bind(shot_ext)
        .bind(meta.to_string())
        .execute(&st.db)
        .await;
        match res {
            Ok(_) => imported += 1,
            Err(e) => warn!("failed to import {folder}: {e}"),
        }
    }
    if imported > 0 {
        info!("[Import] Indexed {imported} existing archive folder(s)");
    }
    Ok(())
}

// ---------- Handlers ----------

async fn index(State(st): State<AppState>) -> ApiResult<Response> {
    match tokio::fs::read(st.data_dir.join("index.html")).await {
        Ok(b) => Ok(([(header::CONTENT_TYPE, "text/html; charset=utf-8")], b).into_response()),
        Err(_) => Err(AppError::not_found("index.html not found")),
    }
}

async fn health(State(st): State<AppState>) -> ApiResult {
    let (urls, versions, pending): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(DISTINCT url) FROM archives),
                (SELECT COUNT(*) FROM archives),
                (SELECT COUNT(*) FROM pending)",
    )
    .fetch_one(&st.db)
    .await?;
    Ok(Json(
        json!({ "status": "online", "urls": urls, "versions": versions, "pending": pending }),
    ))
}

struct Part {
    name: Option<String>,
    filename: Option<String>,
    data: Bytes,
}

/// Accept both field names (html) and file names (page.html).
fn pick<'a>(parts: &'a [Part], names: &[&str]) -> Option<&'a Part> {
    for n in names {
        if let Some(p) = parts.iter().find(|p| p.name.as_deref() == Some(*n)) {
            return Some(p);
        }
    }
    parts
        .iter()
        .find(|p| p.filename.as_deref().is_some_and(|f| names.contains(&f)))
}

fn part_text(p: Option<&Part>) -> String {
    p.map(|p| String::from_utf8_lossy(&p.data).into_owned())
        .unwrap_or_default()
}

async fn archive(
    State(st): State<AppState>,
    mp: Result<Multipart, MultipartRejection>,
) -> ApiResult {
    let mut mp = mp.map_err(|_| AppError::bad("Invalid body. Expected multipart/form-data."))?;

    let mut parts = Vec::new();
    let body_err = |e: axum::extract::multipart::MultipartError| {
        AppError(
            e.status(),
            "Invalid body. Expected multipart/form-data.".into(),
        )
    };
    while let Some(field) = mp.next_field().await.map_err(body_err)? {
        let name = field.name().map(str::to_owned);
        let filename = field.file_name().map(str::to_owned);
        let data = field.bytes().await.map_err(body_err)?;
        parts.push(Part {
            name,
            filename,
            data,
        });
    }

    let html = pick(&parts, &["html", "page.html"]).map(|p| p.data.clone());
    let screenshot =
        pick(&parts, &["screenshot", "screenshot.png", "screenshot.jpg"]).map(|p| p.data.clone());
    let mut screenshot_ext: Option<&str> = None;
    let meta: Option<Value> = {
        let t = part_text(pick(&parts, &["metadata", "json", "metadata.json"]));
        serde_json::from_str(&t)
            .ok()
            .filter(|v: &Value| !v.is_null())
    };
    let meta_str = |k: &str| {
        meta.as_ref()
            .and_then(|m| m.get(k))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    let form_url = part_text(pick(&parts, &["url"]));
    let raw_url = if form_url.is_empty() {
        meta_str("url").unwrap_or_default()
    } else {
        form_url
    };
    let url = normalize_url(&raw_url)
        .ok_or_else(|| AppError::bad("Missing or invalid target URL in archive request"))?;

    let form_title = part_text(pick(&parts, &["title"]));
    let title = if !form_title.is_empty() {
        form_title
    } else {
        meta_str("title").unwrap_or_else(|| url.clone())
    };

    // Tags come from the form field or the metadata; they apply to the URL, not this version.
    let tags = match meta.as_ref().and_then(|m| m.get("tags")) {
        Some(v) => parse_tags(v.clone()),
        None => parse_tags(Value::String(part_text(pick(&parts, &["tags"])))),
    };

    let now = Utc::now();
    let archived_at = iso(now);
    let host: String = url::Url::parse(&url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let folder = format!("{}_{}", archived_at.replace([':', '.'], "-"), host);
    let dir = st.archives_dir.join(&folder);
    tokio::fs::create_dir_all(&dir).await?;

    if let Some(h) = &html {
        tokio::fs::write(dir.join("page.html"), h).await?;
    }
    if let Some(s) = &screenshot {
        // "png" | "jpg" (served filename extension): anything but "jpeg" falls
        // back to png instead of silently not saving the file.
        let ext = if part_text(pick(&parts, &["screenshot-format"])).trim() == "jpeg" {
            "jpg"
        } else {
            "png"
        };
        if ext == "jpg" {
            tokio::fs::write(dir.join("screenshot.jpg"), s).await?;
        } else {
            tokio::fs::write(dir.join("screenshot.png"), s).await?;
        }
        screenshot_ext = Some(ext);
    }
    if let Some(m) = &meta {
        tokio::fs::write(
            dir.join("metadata.json"),
            serde_json::to_vec_pretty(m).unwrap(),
        )
        .await?;
    }

    let id = uuid::Uuid::new_v4().to_string();
    let mut tx = st.db.begin().await?;
    sqlx::query(
        "INSERT INTO archives (id, url, title, folder, archived_at, html_size, has_screenshot, screenshot_ext, metadata)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&url)
    .bind(&title)
    .bind(&folder)
    .bind(&archived_at)
    .bind(html.as_ref().map_or(0, |h| h.len() as i64))
    .bind(screenshot.is_some() as i64)
    .bind(screenshot_ext)
    .bind(meta.as_ref().map(|m| m.to_string()))
    .execute(&mut *tx)
    .await?;
    // Queued tags (if this URL was in the pending queue) merge with given ones;
    // must be read BEFORE the pending row is deleted below.
    let queued_tags: Option<String> = sqlx::query_scalar("SELECT tags FROM pending WHERE url = ?")
        .bind(&url)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    sqlx::query("DELETE FROM pending WHERE url = ?")
        .bind(&url)
        .execute(&mut *tx)
        .await?;
    // Unknown tags among the given ones (plus any tags queued with this URL in
    // the pending queue) are created here, then attached to the website.
    let mut all_tags = tags.clone();
    if let Some(t) = queued_tags {
        all_tags.extend(parse_tags(Value::String(t)));
    }
    if !all_tags.is_empty() {
        let ids = ensure_tag_ids(&mut tx, all_tags).await?;
        for tid in &ids {
            sqlx::query("INSERT OR IGNORE INTO site_tags (url, tag_id) VALUES (?, ?)")
                .bind(&url)
                .bind(tid)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;

    let version_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archives WHERE url = ?")
        .bind(&url)
        .fetch_one(&st.db)
        .await?;
    info!("[Archived] \"{title}\" -> {folder} ({version_count} versions)");

    Ok(Json(json!({
        "success": true,
        "message": "Page archived successfully",
        "id": id,
        "url": url,
        "archivedAt": archived_at,
        "versionCount": version_count,
        "folder": folder,
    })))
}

async fn check(State(st): State<AppState>, Query(q): Query<UrlQuery>) -> ApiResult {
    let raw = q
        .url
        .ok_or_else(|| AppError::bad("Missing \"url\" query parameter"))?;
    let key = normalize_url(&raw).unwrap_or(raw);
    let versions = versions_for_url(&st.db, &key).await?;
    let tags = tags_for_url(&st.db, &key).await?;
    Ok(Json(json!({
        "archived": !versions.is_empty(),
        "count": versions.len(),
        "lastArchivedAt": versions.first().map(|v| &v.archived_at),
        "tags": tags_json(tags),
        "versions": versions.iter().map(Version::from).collect::<Vec<_>>(),
    })))
}

async fn list_archives(State(st): State<AppState>) -> ApiResult {
    let rows: Vec<ArchiveRow> = sqlx::query_as("SELECT * FROM archives ORDER BY archived_at DESC")
        .fetch_all(&st.db)
        .await?;

    // Group by URL, preserving first-seen (newest-first) order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<ArchiveRow>> = HashMap::new();
    for r in rows {
        if !groups.contains_key(&r.url) {
            order.push(r.url.clone());
        }
        groups.entry(r.url.clone()).or_default().push(r);
    }
    // Tags per site (same list for every version of the URL).
    let mut site_tags: HashMap<String, Vec<Value>> = HashMap::new();
    for url in &order {
        site_tags.insert(url.clone(), tags_json(tags_for_url(&st.db, url).await?));
    }
    let items: Vec<Value> = order
        .iter()
        .map(|url| {
            let rows = &groups[url];
            json!({
                "url": url,
                "title": rows[0].title,
                "count": rows.len(),
                "lastArchived": rows[0].archived_at,
                "tags": site_tags.get(url).map(|v| v.as_slice()).unwrap_or(&[]),
                "versions": rows.iter().map(Version::from).collect::<Vec<_>>(),
            })
        })
        .collect();
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

async fn delete_by_url(
    State(st): State<AppState>,
    Query(q): Query<UrlQuery>,
) -> ApiResult<Response> {
    let raw = q
        .url
        .ok_or_else(|| AppError::bad("Missing \"url\" query parameter"))?;
    let key = normalize_url(&raw).unwrap_or(raw);
    let rows = versions_for_url(&st.db, &key).await?;
    if rows.is_empty() {
        return Err(AppError::not_found("URL not archived"));
    }
    let removed = remove_versions(&st, &rows).await?;
    Ok(Json(json!({ "success": true, "removed": removed })).into_response())
}

async fn delete_version(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let row: ArchiveRow = sqlx::query_as("SELECT * FROM archives WHERE id = ?")
        .bind(&id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(|| AppError::not_found("Version not found"))?;
    remove_versions(&st, std::slice::from_ref(&row)).await?;
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM archives WHERE url = ?")
        .bind(&row.url)
        .fetch_one(&st.db)
        .await?;
    Ok(Json(json!({ "success": true, "remaining": remaining })))
}

async fn list_pending(State(st): State<AppState>) -> ApiResult {
    let items: Vec<PendingItem> =
        sqlx::query_as::<_, PendingRow>("SELECT * FROM pending ORDER BY added_at DESC")
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .map(Into::into)
            .collect();
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

async fn add_pending(
    State(st): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let body: Option<Value> = serde_json::from_slice(&body).ok();
    let url = body
        .as_ref()
        .and_then(|b| b.get("url"))
        .and_then(Value::as_str)
        .and_then(normalize_url)
        .ok_or_else(|| AppError::bad("A valid http(s) URL is required"))?;
    let title = body
        .as_ref()
        .and_then(|b| b.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();

    sqlx::query(
        "INSERT INTO pending (id, url, title, added_at, tags) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(url) DO UPDATE SET
           title = CASE WHEN excluded.title != '' THEN excluded.title ELSE pending.title END,
           tags  = CASE WHEN json_array_length(excluded.tags) > 0 THEN excluded.tags ELSE pending.tags END",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&url)
    .bind(&title)
    .bind(iso(Utc::now()))
    .bind(serde_json::to_string(&parse_tags(
        body.and_then(|v|v.get("tags").cloned()).unwrap_or(Value::Null),
    ))
    .unwrap())
    .execute(&st.db)
    .await?;
    let row: PendingRow = sqlx::query_as("SELECT * FROM pending WHERE url = ?")
        .bind(&url)
        .fetch_one(&st.db)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "success": true, "item": PendingItem::from(row) })),
    ))
}

/// PATCH/PUT /api/pending/{id} — replace the tags queued with a URL.
async fn update_pending_tags(
    State(st): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult {
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM pending WHERE id = ?")
        .bind(&id)
        .fetch_optional(&st.db)
        .await?;
    if existing.is_none() {
        return Err(AppError::not_found("Pending item not found"));
    }
    let body: Value = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::bad("Invalid JSON payload"))?;
    let names = parse_tags(body.get("tags").cloned().unwrap_or(Value::Null));
    let stored = if names.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&names).unwrap())
    };
    sqlx::query("UPDATE pending SET tags = ? WHERE id = ?")
        .bind(stored)
        .bind(&id)
        .execute(&st.db)
        .await?;
    let row: PendingRow = sqlx::query_as("SELECT * FROM pending WHERE id = ?")
        .bind(&id)
        .fetch_one(&st.db)
        .await?;
    Ok(Json(
        json!({ "success": true, "item": PendingItem::from(row) }),
    ))
}

async fn complete_pending(State(st): State<AppState>, body: Bytes) -> ApiResult {
    let body = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::bad("Invalid JSON payload"))?;

    if let Some(id) = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        sqlx::query("DELETE FROM pending WHERE id = ?")
            .bind(id)
            .execute(&st.db)
            .await?;
    }
    if let Some(raw) = body
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        let key = normalize_url(raw).unwrap_or_else(|| raw.to_owned());
        sqlx::query("DELETE FROM pending WHERE url = ?")
            .bind(key)
            .execute(&st.db)
            .await?;
    }
    let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pending")
        .fetch_one(&st.db)
        .await?;
    Ok(Json(json!({ "success": true, "remaining": remaining })))
}

async fn delete_pending(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let res = sqlx::query("DELETE FROM pending WHERE id = ?")
        .bind(&id)
        .execute(&st.db)
        .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::not_found("Pending item not found"));
    }
    Ok(Json(json!({ "success": true })))
}

// ---------- tag handlers ----------

#[derive(Debug, FromRow)]
struct TagUsageRow {
    id: String,
    name: String,
    usage: i64,
}

async fn list_tags(State(st): State<AppState>) -> ApiResult {
    let rows: Vec<TagUsageRow> = sqlx::query_as(
        "SELECT t.id, t.name, COUNT(st.url) AS usage
         FROM tags t LEFT JOIN site_tags st ON st.tag_id = t.id
         GROUP BY t.id ORDER BY t.name COLLATE NOCASE",
    )
    .fetch_all(&st.db)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|t| json!({ "id": t.id, "name": t.name, "usage": t.usage }))
        .collect();
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

/// Read a trimmed tag name from a JSON body ({ "name": ... }), 400 if empty.
async fn tag_name_from_body(body: Bytes) -> Result<String, AppError> {
    let body: Value = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::bad("Invalid JSON payload"))?;
    let name = clean_tag_name(body.get("name").and_then(Value::as_str).unwrap_or(""));
    if name.is_empty() {
        return Err(AppError::bad("A non-empty \"name\" is required"));
    }
    Ok(name)
}

async fn create_tag(
    State(st): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = tag_name_from_body(body).await?;
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM tags WHERE name = ? COLLATE NOCASE")
            .bind(&name)
            .fetch_optional(&st.db)
            .await?;
    if existing.is_some() {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("Tag \"{name}\" already exists"),
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO tags (id, name) VALUES (?, ?)")
        .bind(&id)
        .bind(&name)
        .execute(&st.db)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "success": true, "tag": { "id": id, "name": name, "usage": 0 } })),
    ))
}

async fn update_tag(State(st): State<AppState>, Path(id): Path<String>, body: Bytes) -> ApiResult {
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM tags WHERE id = ?")
        .bind(&id)
        .fetch_optional(&st.db)
        .await?;
    if existing.is_none() {
        return Err(AppError::not_found("Tag not found"));
    }
    let name = tag_name_from_body(body).await?;
    let clash: Option<String> =
        sqlx::query_scalar("SELECT id FROM tags WHERE name = ? COLLATE NOCASE")
            .bind(&name)
            .fetch_optional(&st.db)
            .await?;
    if clash.is_some_and(|other| other != id) {
        return Err(AppError(
            StatusCode::CONFLICT,
            format!("Tag \"{name}\" already exists"),
        ));
    }
    sqlx::query("UPDATE tags SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(&id)
        .execute(&st.db)
        .await?;
    Ok(Json(
        json!({ "success": true, "tag": { "id": id, "name": name } }),
    ))
}

async fn delete_tag(State(st): State<AppState>, Path(id): Path<String>) -> ApiResult {
    let existing: Option<String> = sqlx::query_scalar("SELECT id FROM tags WHERE id = ?")
        .bind(&id)
        .fetch_optional(&st.db)
        .await?;
    if existing.is_none() {
        return Err(AppError::not_found("Tag not found"));
    }
    let mut tx = st.db.begin().await?;
    sqlx::query("DELETE FROM site_tags WHERE tag_id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM tags WHERE id = ?")
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "success": true })))
}

/// PUT /api/sites/tags { url, tags } — replace the tag set of a website.
async fn set_site_tags_handler(State(st): State<AppState>, body: Bytes) -> ApiResult {
    let body: Value = serde_json::from_slice::<Value>(&body)
        .ok()
        .filter(|v| !v.is_null())
        .ok_or_else(|| AppError::bad("Invalid JSON payload"))?;
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .and_then(normalize_url)
        .ok_or_else(|| AppError::bad("A valid http(s) URL is required"))?;
    let names = parse_tags(body.get("tags").cloned().unwrap_or(Value::Null));
    let tags = set_site_tags(&st.db, &url, names).await?;
    Ok(Json(json!({ "success": true, "url": url, "tags": tags })))
}

async fn serve_archive_file(
    State(st): State<AppState>,
    Path((folder, file)): Path<(String, String)>,
) -> ApiResult<Response> {
    let content_type = match file.as_str() {
        "page.html" => "text/html; charset=utf-8",
        "screenshot.png" => "image/png",
        "screenshot.jpg" => "image/jpeg",
        "metadata.json" => "application/json",
        _ => return Err(AppError::not_found("File not found")),
    };
    if folder == "."
        || folder == ".."
        || !folder
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return Err(AppError::not_found("File not found"));
    }
    let bytes = tokio::fs::read(st.archives_dir.join(&folder).join(&file))
        .await
        .map_err(|_| AppError::not_found("File not found"))?;

    let mut res = ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Archived pages are untrusted: no scripts, no same-origin access to this server's API.
    if file == "page.html" {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("sandbox"),
        );
    }
    Ok(res)
}

async fn not_found(uri: Uri) -> AppError {
    AppError::not_found(format!("Endpoint not found: {}", uri.path()))
}

async fn log_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let res = next.run(req).await;
    info!("{method} {path} {}", res.status().as_u16());
    res
}

// ---------- Main ----------

fn env_parse<T: FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    info!("shutting down");
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let port: u16 = env_parse("PORT", 3000);
    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let max_body: usize = env_parse::<usize>("MAX_BODY_MB", 256) * 1024 * 1024;

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| ".".into()));
    tokio::fs::create_dir_all(&data_dir).await?;
    let data_dir = tokio::fs::canonicalize(&data_dir).await?;
    let archives_dir = data_dir.join("archives");
    tokio::fs::create_dir_all(&archives_dir).await?;

    let db_path = data_dir.join("db.sqlite");
    let db = init_db(&db_path).await?;

    let state = AppState {
        db: db.clone(),
        data_dir: Arc::new(data_dir),
        archives_dir: Arc::new(archives_dir.clone()),
    };
    import_existing_folders(&state).await.map_err(|e| e.1)?;

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            header::AUTHORIZATION,
            HeaderName::from_static("x-api-key"),
        ]);

    let router = Router::new()
        .route("/", get(index))
        .route("/api/health", get(health))
        .route("/api/check", get(check))
        .route("/api/status", get(check))
        .route("/api/archive", post(archive))
        .route("/api/archives", get(list_archives).delete(delete_by_url))
        .route("/api/archives/{id}", delete(delete_version))
        .route("/api/pending", get(list_pending).post(add_pending))
        .route("/api/pending/complete", post(complete_pending))
        .route(
            "/api/pending/{id}",
            axum::routing::patch(update_pending_tags)
                .put(update_pending_tags)
                .delete(delete_pending),
        )
        .route("/api/tags", get(list_tags).post(create_tag))
        .route(
            "/api/tags/{id}",
            axum::routing::patch(update_tag).delete(delete_tag),
        )
        .route("/api/sites/tags", axum::routing::put(set_site_tags_handler))
        .route("/archives/{folder}/{file}", get(serve_archive_file))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(max_body))
        .layer(middleware::from_fn(log_requests))
        .layer(cors)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    info!("🚀 Web archive server: http://{}", listener.local_addr()?);
    info!("   Archives: {}", archives_dir.display());
    info!("   Database: {}", db_path.display());

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    db.close().await;
    Ok(())
}
