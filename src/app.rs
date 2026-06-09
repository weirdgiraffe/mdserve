use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::get,
    Router,
};
use minijinja::{context, value::Value, Environment};
use notify::{Config, Event as NotifyEvent, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{broadcast, mpsc},
};
use tokio_stream::{wrappers::BroadcastStream, StreamExt as _};
use tower_http::cors::CorsLayer;

const TEMPLATE_NAME: &str = "main.html";
static TEMPLATE_ENV: OnceLock<Environment<'static>> = OnceLock::new();
const MERMAID_JS: &str = include_str!("../static/js/mermaid.min.js");
const MERMAID_ETAG: &str = concat!("\"", env!("CARGO_PKG_VERSION"), "\"");
const MAX_PORT_ATTEMPTS: u16 = 10;
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

fn template_env() -> &'static Environment<'static> {
    TEMPLATE_ENV.get_or_init(|| {
        let mut env = Environment::new();
        minijinja_embed::load_templates!(&mut env);
        env
    })
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Server state. The cache is permanent and unbounded; `open_files` and
/// `watched_dirs` are mutated only by the SSE open/close lifecycle.
struct AppState {
    /// Security boundary, canonicalized at startup. Nothing outside is served.
    base_dir: PathBuf,
    inner: Mutex<Inner>,
    /// Reload signal carrying the relpath of the file that changed.
    reload_tx: broadcast::Sender<String>,
    /// Single watcher, created watching nothing; dirs are added lazily.
    watcher: Mutex<RecommendedWatcher>,
}

struct Inner {
    /// relpath -> rendered body HTML. Permanent, no eviction.
    cache: HashMap<String, String>,
    /// relpath -> count of open SSE streams. Source of truth for "interesting".
    open_files: HashMap<String, usize>,
    /// absolute dir -> count of open files living under it.
    watched_dirs: HashMap<PathBuf, usize>,
}

impl AppState {
    /// Register an open SSE stream for `rel` whose file lives in `dir`.
    /// Watches `dir` on the first open file under it.
    fn open_file(&self, rel: &str, dir: &Path) {
        let mut inner = self.inner.lock().unwrap();
        let count = inner.open_files.entry(rel.to_string()).or_insert(0);
        *count += 1;
        if *count == 1 {
            let dir_count = inner.watched_dirs.entry(dir.to_path_buf()).or_insert(0);
            *dir_count += 1;
            if *dir_count == 1 {
                let mut watcher = self.watcher.lock().unwrap();
                let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
            }
        }
    }

    /// Deregister an open SSE stream for `rel`. Unwatches `dir` when the last
    /// open file under it closes.
    fn close_file(&self, rel: &str, dir: &Path) {
        let mut inner = self.inner.lock().unwrap();
        let removed = match inner.open_files.get_mut(rel) {
            Some(count) => {
                *count -= 1;
                *count == 0
            }
            None => false,
        };
        if !removed {
            return;
        }
        inner.open_files.remove(rel);
        let unwatch = match inner.watched_dirs.get_mut(dir) {
            Some(dir_count) => {
                *dir_count -= 1;
                *dir_count == 0
            }
            None => false,
        };
        if unwatch {
            inner.watched_dirs.remove(dir);
            let mut watcher = self.watcher.lock().unwrap();
            let _ = watcher.unwatch(dir);
        }
    }
}

/// RAII guard living inside the SSE stream. Dropping the stream (tab closed,
/// reload, crash) deregisters the open file.
struct OpenGuard {
    state: Arc<AppState>,
    rel: String,
    dir: PathBuf,
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        self.state.close_file(&self.rel, &self.dir);
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Resolved {
    Directory(PathBuf),
    Markdown { abs: PathBuf, rel: String },
    Image(PathBuf),
    Forbidden,
    NotFound,
}

/// Resolve a request path under `base_dir`. The single entry point used by
/// every content route.
fn resolve(base_dir: &Path, request_path: &str) -> Resolved {
    let trimmed = request_path.trim_start_matches('/');

    // Reject lexical `..` escape attempts before touching the filesystem, so a
    // traversal attempt is forbidden even when the target is missing.
    if trimmed.split('/').any(|c| c == "..") {
        return Resolved::Forbidden;
    }

    let joined = base_dir.join(trimmed);
    let canonical = match joined.canonicalize() {
        Ok(c) => c,
        Err(_) => return Resolved::NotFound,
    };

    // Catch symlink escapes: a canonical path outside the fence is forbidden.
    if !canonical.starts_with(base_dir) {
        return Resolved::Forbidden;
    }

    if canonical.is_dir() {
        return Resolved::Directory(canonical);
    }

    let rel = rel_path(base_dir, &canonical);
    if is_markdown_file(&canonical) {
        Resolved::Markdown {
            abs: canonical,
            rel,
        }
    } else if is_image_file(&canonical.to_string_lossy()) {
        Resolved::Image(canonical)
    } else {
        Resolved::NotFound
    }
}

/// Normalized relative path: forward slashes, no leading `./`. Empty for
/// `base_dir` itself.
fn rel_path(base_dir: &Path, abs: &Path) -> String {
    abs.strip_prefix(base_dir)
        .unwrap_or(abs)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn is_markdown_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("markdown"))
        .unwrap_or(false)
}

/// Compute the initial browser URL path for `path`, validating that it exists
/// and resolves inside `base_dir`.
pub(crate) fn initial_url_path(base_dir: &Path, path: &Path) -> Result<String> {
    let abs = path
        .canonicalize()
        .with_context(|| format!("path does not exist: {}", path.display()))?;
    if !abs.starts_with(base_dir) {
        anyhow::bail!(
            "path {} is outside base-dir {}",
            abs.display(),
            base_dir.display()
        );
    }
    let rel = rel_path(base_dir, &abs);
    Ok(if rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{rel}")
    })
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn markdown_to_html(content: &str) -> String {
    let mut options = markdown::Options::gfm();
    options.compile.allow_dangerous_html = true;
    options.parse.constructs.frontmatter = true;

    markdown::to_html_with_options(content, &options)
        .unwrap_or_else(|_| "Error parsing markdown".to_string())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Render the page template around a body of (already safe) HTML.
fn render_page(body_html: &str, reload_file: Option<&str>, page_title: &str) -> Response {
    let env = template_env();
    let template = match env.get_template(TEMPLATE_NAME) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(format!("Template error: {e}")),
            )
                .into_response();
        }
    };

    let mermaid_enabled = body_html.contains(r#"class="language-mermaid""#);

    match template.render(context! {
        content => Value::from_safe_string(body_html.to_string()),
        mermaid_enabled => mermaid_enabled,
        page_title => page_title,
        reload_file => reload_file,
    }) {
        Ok(rendered) => (StatusCode::OK, Html(rendered)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("Rendering error: {e}")),
        )
            .into_response(),
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Html("<h1>404 Not Found</h1>".to_string()),
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Html("<h1>403 Forbidden</h1>".to_string()),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Content handlers
// ---------------------------------------------------------------------------

async fn serve_root(State(state): State<Arc<AppState>>) -> Response {
    serve_resolved(&state, "")
}

async fn serve_path(
    AxumPath(path): AxumPath<String>,
    State(state): State<Arc<AppState>>,
) -> Response {
    serve_resolved(&state, &path)
}

fn serve_resolved(state: &AppState, request_path: &str) -> Response {
    match resolve(&state.base_dir, request_path) {
        Resolved::Forbidden => forbidden(),
        Resolved::NotFound => not_found(),
        Resolved::Directory(abs) => render_listing(state, &abs),
        Resolved::Image(abs) => serve_image(&abs),
        Resolved::Markdown { abs, rel } => serve_markdown_page(state, &abs, &rel),
    }
}

fn serve_markdown_page(state: &AppState, abs: &Path, rel: &str) -> Response {
    let cached = {
        let inner = state.inner.lock().unwrap();
        inner.cache.get(rel).cloned()
    };

    let body = match cached {
        Some(html) => html,
        None => {
            // Render outside the lock, then store briefly under it.
            let content = match fs::read_to_string(abs) {
                Ok(c) => c,
                Err(_) => return not_found(),
            };
            let html = markdown_to_html(&content);
            let mut inner = state.inner.lock().unwrap();
            inner.cache.entry(rel.to_string()).or_insert(html).clone()
        }
    };

    let page_title = Path::new(rel)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel);

    render_page(&body, Some(rel), page_title)
}

/// Render a vim/netrw-style listing: subdirectories then `.md`/`.markdown`
/// files, each group alphabetical, hrefs absolute from the root. Rendered
/// fresh on every request: no cache, no watch, no SSE.
fn render_listing(state: &AppState, abs_dir: &Path) -> Response {
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();

    let entries = match fs::read_dir(abs_dir) {
        Ok(e) => e,
        Err(_) => return not_found(),
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip dotfiles / dotdirs
        }
        let path = entry.path();
        if path.is_dir() {
            dirs.push(name);
        } else if is_markdown_file(&path) {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();

    let dir_rel = rel_path(&state.base_dir, abs_dir);
    let prefix = if dir_rel.is_empty() {
        "/".to_string()
    } else {
        format!("/{dir_rel}/")
    };

    let mut html = String::from("<ul class=\"listing\">");
    if abs_dir != state.base_dir {
        let parent_href = match dir_rel.rsplit_once('/') {
            Some((parent, _)) => format!("/{parent}/"),
            None => "/".to_string(),
        };
        html.push_str(&format!("<li><a href=\"{parent_href}\">../</a></li>"));
    }
    for d in &dirs {
        let name = html_escape(d);
        html.push_str(&format!("<li><a href=\"{prefix}{name}/\">{name}/</a></li>"));
    }
    for f in &files {
        let name = html_escape(f);
        html.push_str(&format!("<li><a href=\"{prefix}{name}\">{name}</a></li>"));
    }
    html.push_str("</ul>");

    let title = if dir_rel.is_empty() { "/" } else { &dir_rel };
    render_page(&html, None, title)
}

fn serve_image(abs: &Path) -> Response {
    let bytes = match fs::read(abs) {
        Ok(b) => b,
        Err(_) => return not_found(),
    };
    let content_type = guess_image_content_type(&abs.to_string_lossy());
    let mut response = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, content_type.as_str())],
        bytes,
    )
        .into_response();
    if content_type == "image/svg+xml" {
        // An inline-script SVG opened directly must not execute.
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("script-src 'none'"),
        );
    }
    response
}

fn is_image_file(file_path: &str) -> bool {
    guess_image_content_type(file_path).starts_with("image/")
}

fn guess_image_content_type(file_path: &str) -> String {
    let extension = Path::new(file_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");

    match extension.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// Mermaid asset
// ---------------------------------------------------------------------------

async fn serve_mermaid_js(headers: HeaderMap) -> impl IntoResponse {
    if is_etag_match(&headers) {
        return mermaid_response(StatusCode::NOT_MODIFIED, None);
    }
    mermaid_response(StatusCode::OK, Some(MERMAID_JS))
}

fn is_etag_match(headers: &HeaderMap) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|etags| etags.split(',').any(|tag| tag.trim() == MERMAID_ETAG))
}

fn mermaid_response(status: StatusCode, body: Option<&'static str>) -> impl IntoResponse {
    let headers = [
        (header::CONTENT_TYPE, "application/javascript"),
        (header::ETAG, MERMAID_ETAG),
        (header::CACHE_CONTROL, "public, no-cache"),
    ];

    match body {
        Some(content) => (status, headers, content).into_response(),
        None => (status, headers).into_response(),
    }
}

// ---------------------------------------------------------------------------
// SSE live reload
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct EventsQuery {
    file: String,
}

async fn sse_handler(
    Query(query): Query<EventsQuery>,
    State(state): State<Arc<AppState>>,
) -> Response {
    let (abs, rel) = match resolve(&state.base_dir, &query.file) {
        Resolved::Markdown { abs, rel } => (abs, rel),
        Resolved::Forbidden => return forbidden(),
        _ => return not_found(),
    };
    let dir = abs
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state.base_dir.clone());

    // (a) Subscribe first so the background re-render's reload can't be missed.
    let rx = state.reload_tx.subscribe();
    // (b) Register the open file and start the watch if this is the first one.
    state.open_file(&rel, &dir);
    // (c) Re-render in the background to catch edits made while the file was
    //     closed (and its watch gone); reload only if the HTML changed.
    {
        let state = state.clone();
        let rel = rel.clone();
        tokio::spawn(async move {
            rerender_and_notify(&state, &rel, &abs);
        });
    }

    let guard = OpenGuard {
        state: state.clone(),
        rel: rel.clone(),
        dir,
    };

    let stream = BroadcastStream::new(rx).filter_map(move |msg| {
        // Hold the guard alive for the lifetime of the stream so its Drop
        // (the close signal) fires when the stream is dropped.
        let _ = &guard;
        match msg {
            Ok(changed) if changed == rel => Some(Ok::<Event, Infallible>(
                Event::default().event("reload").data("{}"),
            )),
            _ => None,
        }
    });

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(KEEPALIVE_INTERVAL)
                .text("keepalive"),
        )
        .into_response()
}

/// Re-render `rel` from disk and, if the HTML differs from the cache, store it
/// and broadcast a reload. A read failure is a no-op (deletion leaves the page).
fn rerender_and_notify(state: &AppState, rel: &str, abs: &Path) {
    let content = match fs::read_to_string(abs) {
        Ok(c) => c,
        Err(_) => return,
    };
    let html = markdown_to_html(&content);

    let changed = {
        let mut inner = state.inner.lock().unwrap();
        match inner.cache.get(rel) {
            Some(existing) if *existing == html => false,
            _ => {
                inner.cache.insert(rel.to_string(), html);
                true
            }
        }
    };

    if changed {
        let _ = state.reload_tx.send(rel.to_string());
    }
}

// ---------------------------------------------------------------------------
// Watcher
// ---------------------------------------------------------------------------

async fn watcher_loop(state: Arc<AppState>, mut rx: mpsc::Receiver<NotifyEvent>) {
    while let Some(event) = rx.recv().await {
        handle_file_event(&state, event);
    }
}

/// Filter a filesystem event down to a re-render. Only a reappearing path
/// (`create`, rename-arrival) or a data `modify` triggers work; bare removes do
/// nothing, so a genuine deletion leaves the open page as-is.
fn handle_file_event(state: &AppState, event: NotifyEvent) {
    use notify::event::{ModifyKind, RenameMode};

    // Collect the paths that represent a reappearing or modified file. Bare
    // removes (RenameMode::From, Remove, ...) contribute nothing.
    let candidates: Vec<&Path> = match event.kind {
        // Linux/Windows rename-over: [old, new] in one event; the new path.
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => event
            .paths
            .get(1)
            .map(PathBuf::as_path)
            .into_iter()
            .collect(),
        // Rename arrival: a single new path.
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => event
            .paths
            .first()
            .map(PathBuf::as_path)
            .into_iter()
            .collect(),
        // macOS: separate events for old and new; only the one that exists.
        EventKind::Modify(ModifyKind::Name(RenameMode::Any)) => event
            .paths
            .first()
            .filter(|p| p.exists())
            .map(PathBuf::as_path)
            .into_iter()
            .collect(),
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Data(_)) => {
            event.paths.iter().map(PathBuf::as_path).collect()
        }
        _ => Vec::new(),
    };

    for path in candidates {
        maybe_rerender(state, path);
    }
}

/// Re-render `path` only if it is a markdown file currently open in a browser.
fn maybe_rerender(state: &AppState, path: &Path) {
    if !is_markdown_file(path) {
        return;
    }
    let canonical = match path.canonicalize() {
        Ok(c) => c,
        Err(_) => return,
    };
    if !canonical.starts_with(&state.base_dir) {
        return;
    }
    let rel = rel_path(&state.base_dir, &canonical);
    let open = state.inner.lock().unwrap().open_files.contains_key(&rel);
    if open {
        rerender_and_notify(state, &rel, &canonical);
    }
}

// ---------------------------------------------------------------------------
// Server wiring
// ---------------------------------------------------------------------------

fn new_router(base_dir: PathBuf) -> Result<(Router, Arc<AppState>)> {
    let base_dir = base_dir.canonicalize()?;

    let (reload_tx, _) = broadcast::channel::<String>(64);
    let (tx, rx) = mpsc::channel::<NotifyEvent>(100);

    // Single watcher, watching nothing until a file is opened.
    let watcher = RecommendedWatcher::new(
        move |res: std::result::Result<NotifyEvent, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.blocking_send(event);
            }
        },
        Config::default(),
    )?;

    let state = Arc::new(AppState {
        base_dir,
        inner: Mutex::new(Inner {
            cache: HashMap::new(),
            open_files: HashMap::new(),
            watched_dirs: HashMap::new(),
        }),
        reload_tx,
        watcher: Mutex::new(watcher),
    });

    tokio::spawn(watcher_loop(state.clone(), rx));

    let router = Router::new()
        .route("/__mdserve/events", get(sse_handler))
        .route("/__mdserve/mermaid.min.js", get(serve_mermaid_js))
        .route("/", get(serve_root))
        .route("/*path", get(serve_path))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    Ok((router, state))
}

async fn bind_with_retry(hostname: &str, port: u16) -> Result<(TcpListener, u16)> {
    let mut last_err = None;
    for offset in 0..MAX_PORT_ATTEMPTS {
        let try_port = match port.checked_add(offset) {
            Some(p) => p,
            None => break,
        };
        match TcpListener::bind((hostname, try_port)).await {
            Ok(listener) => return Ok((listener, try_port)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => last_err = Some(e),
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_err
        .map(|e| anyhow::anyhow!(e))
        .unwrap_or_else(|| anyhow::anyhow!("no valid port in range"))
        .context(format!(
            "could not bind to ports {}--{}",
            port,
            port.saturating_add(MAX_PORT_ATTEMPTS - 1)
        )))
}

pub(crate) async fn serve(
    base_dir: PathBuf,
    url_path: String,
    hostname: impl AsRef<str>,
    port: u16,
    open: bool,
) -> Result<()> {
    let hostname = hostname.as_ref();

    let (router, _state) = new_router(base_dir.clone())?;

    let (listener, actual_port) = bind_with_retry(hostname, port).await?;
    if actual_port != port {
        println!("⚠ Port {port} in use, using {actual_port} instead");
    }

    let listen_addr = format_host(hostname, actual_port);
    let browse_addr = format_host(&browsable_host(hostname), actual_port);

    println!("📁 Base directory: {}", base_dir.display());
    println!("🌐 Server running at: http://{listen_addr}{url_path}");
    println!("⚡ Live reload enabled");
    println!("\nPress Ctrl+C to stop the server");

    if open {
        open_browser(&format!("http://{browse_addr}{url_path}"))?;
    }

    axum::serve(listener, router).await?;

    Ok(())
}

/// Format the host address (hostname + port) for printing.
fn format_host(hostname: &str, port: u16) -> String {
    if hostname.parse::<Ipv6Addr>().is_ok() {
        format!("[{hostname}]:{port}")
    } else {
        format!("{hostname}:{port}")
    }
}

/// Map wildcard bind addresses to loopback so the browser gets a
/// reachable URL.
fn browsable_host(hostname: &str) -> String {
    if hostname
        .parse::<Ipv4Addr>()
        .ok()
        .is_some_and(|ip| ip.is_unspecified())
    {
        "127.0.0.1".into()
    } else if hostname
        .parse::<Ipv6Addr>()
        .ok()
        .is_some_and(|ip| ip.is_unspecified())
    {
        "::1".into()
    } else {
        hostname.into()
    }
}

/// Open a URL in the default browser using platform commands.
fn open_browser(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "linux") {
        "xdg-open"
    } else {
        anyhow::bail!("--open is not supported on this platform");
    };

    let mut child = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;

    std::thread::spawn(move || match child.wait() {
        Ok(status) if !status.success() => {
            eprintln!("{program} exited with {status}");
        }
        Err(e) => eprintln!("Failed waiting on {program}: {e}"),
        _ => {}
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum_test::TestServer;
    use std::net::SocketAddr;
    use std::time::Instant;
    use tempfile::{tempdir, TempDir};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    // -- unit: path / extension helpers -----------------------------------

    #[test]
    fn test_is_markdown_file() {
        assert!(is_markdown_file(Path::new("test.md")));
        assert!(is_markdown_file(Path::new("/path/to/file.markdown")));
        assert!(is_markdown_file(Path::new("test.MD")));
        assert!(is_markdown_file(Path::new("test.MARKDOWN")));

        assert!(!is_markdown_file(Path::new("test.txt")));
        assert!(!is_markdown_file(Path::new("README")));
    }

    #[test]
    fn test_is_image_file() {
        for ext in [
            "png", "jpg", "jpeg", "gif", "svg", "webp", "avif", "bmp", "ico",
        ] {
            assert!(is_image_file(&format!("test.{ext}")), "{ext}");
        }
        assert!(is_image_file("test.PNG"));
        assert!(!is_image_file("test.txt"));
        assert!(!is_image_file("test.md"));
    }

    #[test]
    fn test_guess_image_content_type() {
        assert_eq!(guess_image_content_type("test.png"), "image/png");
        assert_eq!(guess_image_content_type("test.jpg"), "image/jpeg");
        assert_eq!(guess_image_content_type("test.svg"), "image/svg+xml");
        assert_eq!(guess_image_content_type("test.avif"), "image/avif");
        assert_eq!(guess_image_content_type("test.ico"), "image/x-icon");
        assert_eq!(
            guess_image_content_type("test.xyz"),
            "application/octet-stream"
        );
    }

    #[test]
    fn test_format_host() {
        assert_eq!(format_host("127.0.0.1", 3000), "127.0.0.1:3000");
        assert_eq!(format_host("localhost", 3000), "localhost:3000");
        assert_eq!(format_host("::1", 3000), "[::1]:3000");
    }

    #[test]
    fn test_browsable_host() {
        assert_eq!(browsable_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(browsable_host("::"), "::1");
        assert_eq!(browsable_host("127.0.0.1"), "127.0.0.1");
        assert_eq!(browsable_host("example.com"), "example.com");
    }

    #[tokio::test]
    async fn test_bind_retries_on_addr_in_use() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let blocked_port = listener.local_addr().unwrap().port();

        let (retry_listener, actual_port) =
            bind_with_retry("127.0.0.1", blocked_port).await.unwrap();

        assert!(actual_port > blocked_port);

        drop(retry_listener);
        drop(listener);
    }

    // -- unit: resolver ----------------------------------------------------

    #[test]
    fn test_resolve() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        fs::create_dir_all(base.join("docs")).unwrap();
        fs::write(base.join("docs/api.md"), "# api").unwrap();
        fs::write(base.join("pic.png"), [0u8; 4]).unwrap();
        fs::write(base.join("art.avif"), [0u8; 4]).unwrap();
        fs::write(base.join("secret.txt"), "s").unwrap();

        match resolve(&base, "docs/api.md") {
            Resolved::Markdown { rel, .. } => assert_eq!(rel, "docs/api.md"),
            o => panic!("expected markdown, got {o:?}"),
        }
        assert!(matches!(resolve(&base, ""), Resolved::Directory(_)));
        assert!(matches!(resolve(&base, "docs"), Resolved::Directory(_)));
        assert!(matches!(resolve(&base, "../x.md"), Resolved::Forbidden));
        assert!(matches!(
            resolve(&base, "docs/../../x"),
            Resolved::Forbidden
        ));
        assert!(matches!(resolve(&base, "missing.md"), Resolved::NotFound));
        assert!(matches!(resolve(&base, "secret.txt"), Resolved::NotFound));
        assert!(matches!(resolve(&base, "pic.png"), Resolved::Image(_)));
        assert!(matches!(resolve(&base, "art.avif"), Resolved::Image(_)));
    }

    #[test]
    #[cfg(unix)]
    fn test_resolve_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("o.md");
        fs::write(&target, "# o").unwrap();
        symlink(&target, base.join("link.md")).unwrap();
        assert!(matches!(resolve(&base, "link.md"), Resolved::Forbidden));
    }

    #[test]
    fn test_initial_url_path() {
        let dir = tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        fs::create_dir(base.join("docs")).unwrap();
        fs::write(base.join("docs/api.md"), "# x").unwrap();

        assert_eq!(initial_url_path(&base, &base).unwrap(), "/");
        assert_eq!(
            initial_url_path(&base, &base.join("docs/api.md")).unwrap(),
            "/docs/api.md"
        );
        assert_eq!(
            initial_url_path(&base, &base.join("docs")).unwrap(),
            "/docs"
        );

        // A path outside the fence is rejected.
        let outside = dir.path().parent().unwrap().to_path_buf();
        assert!(initial_url_path(&base, &outside).is_err());
    }

    // -- helpers -----------------------------------------------------------

    /// A mock-transport server backed by a tempdir with one `test.md`.
    async fn md_server(content: &str) -> (TestServer, TempDir) {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.md"), content).unwrap();
        let (router, _state) = new_router(dir.path().to_path_buf()).unwrap();
        (TestServer::new(router).unwrap(), dir)
    }

    /// A real TCP server, returning its address and a handle to its state.
    async fn spawn_test_server(dir: &Path) -> (SocketAddr, Arc<AppState>) {
        let (router, state) = new_router(dir.to_path_buf()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (addr, state)
    }

    /// Issue a GET and return the status code (consuming only the status line,
    /// which is enough to guarantee the handler ran and the cache was populated).
    async fn http_get(addr: SocketAddr, path: &str) -> u16 {
        let mut reader = BufReader::new(TcpStream::connect(addr).await.unwrap());
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        reader.get_mut().write_all(req.as_bytes()).await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        line.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    struct SseConn {
        reader: BufReader<TcpStream>,
    }

    /// Open an SSE stream; returns the HTTP status and the live connection.
    /// Dropping the returned `SseConn` closes the socket (the close signal).
    async fn connect_sse(addr: SocketAddr, file: &str) -> (u16, SseConn) {
        let stream = TcpStream::connect(addr).await.unwrap();
        let mut reader = BufReader::new(stream);
        let req = format!(
            "GET /__mdserve/events?file={file} HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n"
        );
        reader.get_mut().write_all(req.as_bytes()).await.unwrap();

        let mut status_line = String::new();
        reader.read_line(&mut status_line).await.unwrap();
        let code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);

        loop {
            let mut header = String::new();
            let n = reader.read_line(&mut header).await.unwrap();
            if n == 0 || header == "\r\n" {
                break;
            }
        }
        (code, SseConn { reader })
    }

    impl SseConn {
        async fn expect_event(&mut self, name: &str, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            let want = format!("event: {name}");
            loop {
                let now = Instant::now();
                assert!(now < deadline, "timed out waiting for {want}");
                let mut line = String::new();
                match tokio::time::timeout(deadline - now, self.reader.read_line(&mut line)).await {
                    Ok(Ok(0)) => panic!("connection closed waiting for {want}"),
                    Ok(Ok(_)) => {
                        if line.trim() == want {
                            return;
                        }
                    }
                    Ok(Err(e)) => panic!("read error: {e}"),
                    Err(_) => panic!("timed out waiting for {want}"),
                }
            }
        }

        async fn expect_no_event(&mut self, name: &str, within: Duration) {
            let deadline = Instant::now() + within;
            let unwanted = format!("event: {name}");
            loop {
                let now = Instant::now();
                if now >= deadline {
                    return;
                }
                let mut line = String::new();
                match tokio::time::timeout(deadline - now, self.reader.read_line(&mut line)).await {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                    Ok(Ok(_)) => assert_ne!(line.trim(), unwanted, "unexpected {unwanted}"),
                }
            }
        }
    }

    async fn wait_for<F: Fn(&Inner) -> bool>(state: &AppState, pred: F, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&state.inner.lock().unwrap()) {
                return;
            }
            assert!(Instant::now() < deadline, "wait_for timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // -- rendering ---------------------------------------------------------

    #[tokio::test]
    async fn test_serves_basic_markdown() {
        let (server, _d) = md_server("# Hello World\n\nThis is **bold** text.").await;
        let response = server.get("/test.md").await;
        assert_eq!(response.status_code(), 200);
        let body = response.text();
        assert!(body.contains("<h1>Hello World</h1>"));
        assert!(body.contains("<strong>bold</strong>"));
        assert!(body.contains("theme-toggle"));
        assert!(body.contains("--bg-color"));
        assert!(body.contains("data-theme=\"dark\""));
    }

    #[tokio::test]
    async fn test_gfm_features() {
        let content = "# GFM\n\n| Name | Age |\n|------|-----|\n| John | 30 |\n\n~~gone~~\n";
        let (server, _d) = md_server(content).await;
        let body = server.get("/test.md").await.text();
        assert!(body.contains("<table>"));
        assert!(body.contains("<th>Name</th>"));
        assert!(body.contains("<td>John</td>"));
        assert!(body.contains("<del>gone</del>"));
    }

    #[tokio::test]
    async fn test_html_passthrough() {
        let content = "# T\n\n<div class=\"highlight\"><p>raw</p></div>\n\n**md**\n";
        let (server, _d) = md_server(content).await;
        let body = server.get("/test.md").await.text();
        assert!(body.contains(r#"<div class="highlight">"#));
        assert!(!body.contains("&lt;div"));
        assert!(body.contains("<strong>md</strong>"));
    }

    #[tokio::test]
    async fn test_yaml_frontmatter_is_stripped() {
        let (server, _d) =
            md_server("---\ntitle: Test Post\nauthor: Name\n---\n\n# Test Post\n").await;
        let body = server.get("/test.md").await.text();
        assert!(!body.contains("author: Name"));
        assert!(body.contains("<h1>Test Post</h1>"));
    }

    #[tokio::test]
    async fn test_toml_frontmatter_is_stripped() {
        let (server, _d) = md_server("+++\ntitle = \"Test Post\"\n+++\n\n# Test Post\n").await;
        let body = server.get("/test.md").await.text();
        assert!(!body.contains("title = \"Test Post\""));
        assert!(body.contains("<h1>Test Post</h1>"));
    }

    #[tokio::test]
    async fn test_mermaid_detection_and_injection() {
        let content = "# M\n\n```mermaid\ngraph TD\n    A --> B\n```\n";
        let (server, _d) = md_server(content).await;
        let body = server.get("/test.md").await.text();
        assert!(body.contains(r#"class="language-mermaid""#));
        assert!(body.contains(r#"<script src="/__mdserve/mermaid.min.js"></script>"#));
        assert!(body.contains("function initMermaid()"));
        assert!(body.contains("function transformMermaidCodeBlocks()"));
    }

    #[tokio::test]
    async fn test_no_mermaid_injection_without_blocks() {
        let content = "# No M\n\n```javascript\nconsole.log(1);\n```\n";
        let (server, _d) = md_server(content).await;
        let body = server.get("/test.md").await.text();
        assert!(!body.contains(r#"<script src="/__mdserve/mermaid.min.js"></script>"#));
        assert!(body.contains(r#"class="language-javascript""#));
    }

    #[tokio::test]
    async fn test_multiple_mermaid_diagrams() {
        let content = "```mermaid\ngraph LR\nA-->B\n```\n\n```mermaid\nsequenceDiagram\nA->>B: hi\n```\n\n```mermaid\nclassDiagram\nAnimal<|--Duck\n```\n";
        let (server, _d) = md_server(content).await;
        let body = server.get("/test.md").await.text();
        assert_eq!(body.matches(r#"class="language-mermaid""#).count(), 3);
        assert_eq!(
            body.matches(r#"<script src="/__mdserve/mermaid.min.js"></script>"#)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn test_mermaid_js_etag_caching() {
        let (server, _d) = md_server("# T").await;

        let response = server.get("/__mdserve/mermaid.min.js").await;
        assert_eq!(response.status_code(), 200);
        let etag = response.header("etag");
        assert!(!etag.is_empty());
        assert_eq!(response.header("content-type"), "application/javascript");
        assert!(!response.as_bytes().is_empty());

        let response_304 = server
            .get("/__mdserve/mermaid.min.js")
            .add_header(
                axum::http::header::IF_NONE_MATCH,
                axum::http::HeaderValue::from_str(etag.to_str().unwrap()).unwrap(),
            )
            .await;
        assert_eq!(response_304.status_code(), 304);
        assert!(response_304.as_bytes().is_empty());
    }

    // -- images / 404 ------------------------------------------------------

    #[tokio::test]
    async fn test_image_serving() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.md"), "![x](test.png)").unwrap();
        let png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        fs::write(dir.path().join("test.png"), png).unwrap();
        let (router, _s) = new_router(dir.path().to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        let response = server.get("/test.png").await;
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.header("content-type"), "image/png");
        assert!(!response.as_bytes().is_empty());
    }

    #[tokio::test]
    async fn test_svg_csp_and_avif() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("index.md"), "# x").unwrap();
        fs::write(dir.path().join("icon.svg"), "<svg></svg>").unwrap();
        fs::write(dir.path().join("img.avif"), [0u8; 4]).unwrap();
        let (router, _s) = new_router(dir.path().to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        let svg = server.get("/icon.svg").await;
        assert_eq!(svg.header("content-type"), "image/svg+xml");
        assert_eq!(svg.header("content-security-policy"), "script-src 'none'");

        let avif = server.get("/img.avif").await;
        assert_eq!(avif.header("content-type"), "image/avif");
    }

    #[tokio::test]
    async fn test_non_markdown_non_image_is_404() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("test.md"), "# x").unwrap();
        fs::write(dir.path().join("secret.txt"), "secret").unwrap();
        let (router, _s) = new_router(dir.path().to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        assert_eq!(server.get("/secret.txt").await.status_code(), 404);
        assert_eq!(server.get("/missing.md").await.status_code(), 404);
    }

    // -- directory listing -------------------------------------------------

    #[tokio::test]
    async fn test_directory_listing() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        fs::create_dir(base.join("docs")).unwrap();
        fs::create_dir(base.join("specs")).unwrap();
        fs::write(base.join("readme.md"), "# r").unwrap();
        fs::write(base.join("docs/a.md"), "# a").unwrap();
        fs::write(base.join("secret.txt"), "s").unwrap();
        fs::write(base.join(".hidden.md"), "# h").unwrap();
        let (router, _s) = new_router(base.to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        let body = server.get("/").await.text();
        let docs = body.find("/docs/").expect("docs");
        let specs = body.find("/specs/").expect("specs");
        let readme = body.find("/readme.md").expect("readme");
        assert!(docs < specs, "dirs alphabetical");
        assert!(specs < readme, "dirs before files");
        assert!(!body.contains("secret.txt"), "non-md not listed");
        assert!(!body.contains(".hidden"), "dotfiles skipped");
        assert!(!body.contains("../"), "root has no parent link");

        let sub = server.get("/docs").await.text();
        assert!(sub.contains("../"), "subdir has parent link");
        assert!(sub.contains(r#"href="/docs/a.md""#), "absolute href");
        // Listings get no live-reload.
        assert!(!sub.contains("new EventSource"));
    }

    #[tokio::test]
    async fn test_markdown_page_has_eventsource_listing_does_not() {
        let (server, _d) = md_server("# Hello").await;
        let page = server.get("/test.md").await.text();
        assert!(page.contains("new EventSource"));
        assert!(page.contains(r#"file=' + encodeURIComponent("test.md")"#));

        let listing = server.get("/").await.text();
        assert!(!listing.contains("new EventSource"));
    }

    // -- cache-first / no eager watch -------------------------------------

    #[tokio::test]
    async fn test_cache_first_without_sse_serves_stale() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# Original").unwrap();
        let (router, _s) = new_router(dir.path().to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        assert!(server.get("/a.md").await.text().contains("Original"));
        fs::write(dir.path().join("a.md"), "# Changed").unwrap();

        // No SSE open -> no watch -> cache still serves the old HTML.
        let second = server.get("/a.md").await.text();
        assert!(second.contains("Original"));
        assert!(!second.contains("Changed"));
    }

    // -- SSE lifecycle -----------------------------------------------------

    #[tokio::test]
    async fn test_sse_refcounting_and_cleanup() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        let (code, c1) = connect_sse(addr, "a.md").await;
        assert_eq!(code, 200);
        wait_for(
            &state,
            |i| i.open_files.get("a.md") == Some(&1) && i.watched_dirs.values().any(|&v| v == 1),
            Duration::from_secs(2),
        )
        .await;

        let (_, c2) = connect_sse(addr, "a.md").await;
        wait_for(
            &state,
            |i| i.open_files.get("a.md") == Some(&2),
            Duration::from_secs(2),
        )
        .await;

        drop(c2);
        wait_for(
            &state,
            |i| i.open_files.get("a.md") == Some(&1) && !i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;

        drop(c1);
        wait_for(
            &state,
            |i| i.open_files.is_empty() && i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;
    }

    #[tokio::test]
    async fn test_sse_reloads_on_stale_cache_then_terminates() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# Original").unwrap();
        let (addr, _state) = spawn_test_server(dir.path()).await;

        // Prime the cache with the original render.
        assert_eq!(http_get(addr, "/a.md").await, 200);
        // Edit while closed (no watch).
        fs::write(dir.path().join("a.md"), "# Changed").unwrap();

        // Open SSE: background re-render sees the diff and reloads.
        let (code, mut c) = connect_sse(addr, "a.md").await;
        assert_eq!(code, 200);
        c.expect_event("reload", Duration::from_secs(3)).await;

        // Reconnect: re-render now matches the cache, so no further reload.
        let (_, mut c2) = connect_sse(addr, "a.md").await;
        c2.expect_no_event("reload", Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn test_sse_missing_and_escape() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        let (addr, _state) = spawn_test_server(dir.path()).await;

        assert_eq!(connect_sse(addr, "missing.md").await.0, 404);
        let escape = connect_sse(addr, "../escape.md").await.0;
        assert!(escape == 403 || escape == 404, "got {escape}");
    }

    // -- watcher -----------------------------------------------------------

    #[tokio::test]
    async fn test_watch_modify_reloads_open_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# Original").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        assert_eq!(http_get(addr, "/a.md").await, 200);
        let (code, mut c) = connect_sse(addr, "a.md").await;
        assert_eq!(code, 200);
        wait_for(
            &state,
            |i| i.watched_dirs.values().any(|&v| v >= 1),
            Duration::from_secs(2),
        )
        .await;

        fs::write(dir.path().join("a.md"), "# Modified").unwrap();
        c.expect_event("reload", Duration::from_secs(5)).await;

        assert!(http_get(addr, "/a.md").await == 200);
        assert!(state
            .inner
            .lock()
            .unwrap()
            .cache
            .get("a.md")
            .unwrap()
            .contains("Modified"));
    }

    #[tokio::test]
    async fn test_watch_ignores_unopened_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        fs::write(dir.path().join("b.md"), "# B").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        assert_eq!(http_get(addr, "/a.md").await, 200);
        let (_, mut c) = connect_sse(addr, "a.md").await;
        wait_for(
            &state,
            |i| !i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;

        fs::write(dir.path().join("b.md"), "# B changed").unwrap();
        c.expect_no_event("reload", Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn test_watch_editor_save_rename_over() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "# Original").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        assert_eq!(http_get(addr, "/a.md").await, 200);
        let (_, mut c) = connect_sse(addr, "a.md").await;
        wait_for(
            &state,
            |i| !i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;

        let tmp = dir.path().join("a.md.tmp");
        fs::write(&tmp, "# Updated via rename").unwrap();
        fs::rename(&tmp, &a).unwrap();
        c.expect_event("reload", Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn test_watch_remove_then_create() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "# Original").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        assert_eq!(http_get(addr, "/a.md").await, 200);
        let (_, mut c) = connect_sse(addr, "a.md").await;
        wait_for(
            &state,
            |i| !i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;

        let backup = dir.path().join("a.md~");
        fs::rename(&a, &backup).unwrap();
        fs::write(&a, "# Recreated content").unwrap();
        c.expect_event("reload", Duration::from_secs(5)).await;
    }

    #[tokio::test]
    async fn test_watch_genuine_deletion_leaves_page() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.md");
        fs::write(&a, "# A").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        assert_eq!(http_get(addr, "/a.md").await, 200);
        let (_, mut c) = connect_sse(addr, "a.md").await;
        wait_for(
            &state,
            |i| !i.watched_dirs.is_empty(),
            Duration::from_secs(2),
        )
        .await;

        fs::remove_file(&a).unwrap();
        // A bare remove triggers nothing.
        c.expect_no_event("reload", Duration::from_secs(1)).await;
        // A manual refresh 404s (resolver checks existence first)...
        assert_eq!(http_get(addr, "/a.md").await, 404);
        // ...but the open file stays until the stream drops.
        assert!(state.inner.lock().unwrap().open_files.contains_key("a.md"));
    }

    #[tokio::test]
    async fn test_watch_subdir_independence() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::create_dir(dir.path().join("specs")).unwrap();
        fs::write(dir.path().join("docs/a.md"), "# A").unwrap();
        fs::write(dir.path().join("specs/b.md"), "# B").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        let (_, c1) = connect_sse(addr, "docs/a.md").await;
        let (_, _c2) = connect_sse(addr, "specs/b.md").await;
        wait_for(
            &state,
            |i| i.watched_dirs.len() == 2,
            Duration::from_secs(2),
        )
        .await;

        drop(c1);
        wait_for(
            &state,
            |i| i.watched_dirs.len() == 1 && !i.open_files.contains_key("docs/a.md"),
            Duration::from_secs(2),
        )
        .await;
    }

    #[tokio::test]
    async fn test_watch_same_dir_shares_one_watch() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.md"), "# A").unwrap();
        fs::write(dir.path().join("b.md"), "# B").unwrap();
        let (addr, state) = spawn_test_server(dir.path()).await;

        let (_, c_a) = connect_sse(addr, "a.md").await;
        let (_, _c_b) = connect_sse(addr, "b.md").await;
        // Two open files under one dir -> a single watched dir, refcount 2.
        wait_for(
            &state,
            |i| i.open_files.len() == 2 && i.watched_dirs.len() == 1,
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            *state
                .inner
                .lock()
                .unwrap()
                .watched_dirs
                .values()
                .next()
                .unwrap(),
            2
        );

        // Closing one leaves the dir watched for its sibling.
        drop(c_a);
        wait_for(
            &state,
            |i| !i.open_files.contains_key("a.md") && i.watched_dirs.len() == 1,
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(
            *state
                .inner
                .lock()
                .unwrap()
                .watched_dirs
                .values()
                .next()
                .unwrap(),
            1
        );
    }

    // -- reserved /__mdserve/ prefix --------------------------------------

    #[tokio::test]
    async fn test_mdserve_prefix_wins_but_other_subpaths_browse() {
        let dir = tempdir().unwrap();
        // A directory literally named __mdserve, with content under it.
        fs::create_dir(dir.path().join("__mdserve")).unwrap();
        fs::write(dir.path().join("__mdserve/note.md"), "# Note").unwrap();
        let (router, _s) = new_router(dir.path().to_path_buf()).unwrap();
        let server = TestServer::new(router).unwrap();

        // The two reserved exact routes win over content.
        assert_eq!(
            server.get("/__mdserve/mermaid.min.js").await.status_code(),
            200
        );
        // Any other path under the prefix is plain content.
        let note = server.get("/__mdserve/note.md").await;
        assert_eq!(note.status_code(), 200);
        assert!(note.text().contains("<h1>Note</h1>"));
    }
}
