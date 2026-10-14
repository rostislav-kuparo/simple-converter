#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod job;
mod media;
mod plan;
mod state;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use state::{App, Item, Status};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::TcpListener;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

const PORT: u16 = 47380;
const UI: &str = include_str!("../ui/index.html");

type Shared = State<Arc<App>>;
type ApiError = (StatusCode, String);

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let server_only = flag("--server");
    let out_dir = args
        .windows(2)
        .find(|w| w[0] == "--out")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| dirs::download_dir().unwrap_or_else(std::env::temp_dir).join("Simple Converter"));

    // One instance is enough: if it is already running, just show its window.
    let (listener, main_port) = match TcpListener::bind(("127.0.0.1", PORT)).await {
        Ok(l) => (l, true),
        Err(_) if is_running(PORT).await => {
            open_window(&format!("http://127.0.0.1:{PORT}/"), flag("--browser"));
            return;
        }
        Err(_) => (TcpListener::bind(("127.0.0.1", 0)).await.expect("cannot open a local port"), false),
    };
    let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());

    let tmp_root = std::env::temp_dir().join("simple-converter");
    if main_port {
        let _ = std::fs::remove_dir_all(&tmp_root); // leftovers of a crashed run
    }
    let tmp_dir = tmp_root.join(std::process::id().to_string());
    let _ = std::fs::create_dir_all(&tmp_dir);

    let app = Arc::new(App::new(media::Tools::find(), out_dir, tmp_dir));
    {
        let app = app.clone();
        tokio::spawn(async move { app.caps().await; }); // GPU probing in the background
    }

    let router = Router::new()
        .route("/", get(index))
        .route("/api/ping", get(|| async { "simple-converter" }))
        .route("/api/caps", get(caps))
        .route("/api/items", get(items))
        .route("/api/events", get(events))
        .route("/api/upload", post(upload))
        .route("/api/start", post(start))
        .route("/api/cancel/{id}", post(cancel))
        .route("/api/cancel-all", post(cancel_all))
        .route("/api/items/{id}", delete(remove))
        .route("/api/clear", post(clear))
        .route("/api/open/{id}/{n}", post(open_output))
        .route("/api/open-folder", post(open_folder))
        .layer(DefaultBodyLimit::disable())
        .with_state(app.clone());

    if server_only {
        println!("Simple Converter: {url}");
    } else {
        open_window(&url, flag("--browser"));
        tokio::spawn(exit_when_idle(app.clone()));
    }
    axum::serve(listener, router).await.expect("server stopped");
}

async fn is_running(port: u16) -> bool {
    let probe = async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        s.write_all(b"GET /api/ping HTTP/1.0\r\nHost: localhost\r\n\r\n").await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        Some(String::from_utf8_lossy(&buf).contains("simple-converter"))
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), probe).await, Ok(Some(true)))
}

/// Opens the UI as a chromeless app window (Chrome/Edge `--app`), or a normal browser tab.
fn open_window(url: &str, plain_browser: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if !plain_browser {
            let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
            let candidates = [
                env("ProgramFiles").map(|p| p.join(r"Google\Chrome\Application\chrome.exe")),
                env("ProgramFiles(x86)").map(|p| p.join(r"Google\Chrome\Application\chrome.exe")),
                env("LOCALAPPDATA").map(|p| p.join(r"Google\Chrome\Application\chrome.exe")),
                env("ProgramFiles(x86)").map(|p| p.join(r"Microsoft\Edge\Application\msedge.exe")),
                env("ProgramFiles").map(|p| p.join(r"Microsoft\Edge\Application\msedge.exe")),
            ];
            for exe in candidates.into_iter().flatten().filter(|p| p.is_file()) {
                let launched = std::process::Command::new(exe)
                    .arg(format!("--app={url}"))
                    .arg("--window-size=760,900")
                    .creation_flags(0x0800_0000)
                    .spawn();
                if launched.is_ok() {
                    return;
                }
            }
        }
        let _ = std::process::Command::new("explorer").arg(url).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = plain_browser;
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let _ = std::process::Command::new(opener).arg(url).spawn();
    }
}

/// The app has no window of its own: once every page is closed and nothing is running, quit.
async fn exit_when_idle(app: Arc<App>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if app.clients.load(Ordering::SeqCst) > 0 || app.active_jobs.load(Ordering::SeqCst) > 0 {
            continue;
        }
        let grace = if app.ever_connected.load(Ordering::SeqCst) { 15 } else { 120 };
        if app.last_seen.lock().unwrap().elapsed() > Duration::from_secs(grace) {
            let _ = std::fs::remove_dir_all(&app.tmp_dir);
            std::process::exit(0);
        }
    }
}

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], UI)
}

async fn caps(State(app): Shared) -> Json<Value> {
    let caps = app.caps().await;
    let short: Vec<String> = app
        .out_dir
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Json(json!({
        "ffmpeg": app.tools.is_some(),
        "hw": caps.hw_name(),
        "av1": caps.av1(),
        "out_dir": app.out_dir,
        "out_short": short.into_iter().rev().collect::<Vec<_>>().join("\\"),
    }))
}

async fn items(State(app): Shared) -> Json<Value> {
    let items = app.items.lock().unwrap();
    let mut list: Vec<&Item> = items.values().collect();
    list.sort_by_key(|it| it.id);
    Json(Value::Array(list.into_iter().map(Item::view).collect()))
}

struct ClientGuard(Arc<App>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::SeqCst);
        *self.0.last_seen.lock().unwrap() = Instant::now();
    }
}

async fn events(State(app): Shared) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    app.clients.fetch_add(1, Ordering::SeqCst);
    app.ever_connected.store(true, Ordering::SeqCst);
    let guard = ClientGuard(app.clone());
    let stream = BroadcastStream::new(app.events.subscribe()).filter_map(move |msg| {
        let _alive = &guard; // dropped together with the stream when the page goes away
        msg.ok().map(|data| Ok(Event::default().data(data)))
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
}

#[derive(Deserialize)]
struct UploadQuery {
    name: String,
    #[serde(default)]
    key: String,
}

async fn upload(State(app): Shared, Query(q): Query<UploadQuery>, body: Body) -> Result<Json<Value>, ApiError> {
    let Some(tools) = app.tools.clone() else {
        return Err((StatusCode::SERVICE_UNAVAILABLE, "FFmpeg не найден".into()));
    };
    let name = state::sanitize_name(&q.name);
    let (_, ext) = state::split_name(&name);
    let id = app.next_id.fetch_add(1, Ordering::SeqCst);
    let dir = app.tmp_dir.join(id.to_string());
    // Our own file name inside the temp folder: FFmpeg treats '%' in input names as patterns.
    let src = dir.join(if ext.is_empty() { "source".into() } else { format!("source.{ext}") });

    let saved = async {
        tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
        let mut file = BufWriter::with_capacity(1 << 20, tokio::fs::File::create(&src).await.map_err(|e| e.to_string())?);
        let mut stream = body.into_data_stream();
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| "Загрузка прервана".to_string())?;
            size += chunk.len() as u64;
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.flush().await.map_err(|e| e.to_string())?;
        Ok::<u64, String>(size)
    }
    .await;
    let size = match saved {
        Ok(size) => size,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err((StatusCode::BAD_REQUEST, e));
        }
    };

    let (kind, info) = match media::probe(&tools, &src, &ext).await {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, e));
        }
    };
    let item = Item {
        id,
        key: q.key,
        name,
        size,
        kind,
        info,
        src,
        status: Status::Ready,
        progress: 0.0,
        speed: None,
        outputs: Vec::new(),
        error: None,
        note: None,
        cancel: CancellationToken::new(),
    };
    let view = item.view();
    app.items.lock().unwrap().insert(id, item);
    app.emit(json!({ "type": "item", "item": view }));
    Ok(Json(view))
}

#[derive(Deserialize)]
struct StartRequest {
    ids: Vec<u64>,
    settings: plan::Settings,
}

async fn start(State(app): Shared, Json(req): Json<StartRequest>) -> StatusCode {
    for id in req.ids {
        let token = CancellationToken::new();
        let queued = app.update(id, |it| {
            if it.busy() {
                return false;
            }
            it.status = Status::Queued;
            it.progress = 0.0;
            it.speed = None;
            it.outputs.clear();
            it.error = None;
            it.note = None;
            it.cancel = token.clone();
            true
        });
        if queued == Some(true) {
            app.active_jobs.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(job::run(app.clone(), id, req.settings.clone(), token));
        }
    }
    StatusCode::NO_CONTENT
}

async fn cancel(State(app): Shared, Path(id): Path<u64>) -> StatusCode {
    if let Some(it) = app.items.lock().unwrap().get(&id) {
        it.cancel.cancel();
    }
    StatusCode::NO_CONTENT
}

async fn cancel_all(State(app): Shared) -> StatusCode {
    for it in app.items.lock().unwrap().values() {
        it.cancel.cancel();
    }
    StatusCode::NO_CONTENT
}

fn forget(app: &App, ids: Vec<u64>) {
    let removed: Vec<Item> = {
        let mut items = app.items.lock().unwrap();
        ids.iter().filter_map(|id| items.remove(id)).collect()
    };
    for it in removed {
        it.cancel.cancel();
        app.emit(json!({ "type": "remove", "id": it.id }));
        let dir = app.tmp_dir.join(it.id.to_string());
        tokio::spawn(async move {
            // A just-killed FFmpeg may hold the file for a moment on Windows.
            for _ in 0..10 {
                if tokio::fs::remove_dir_all(&dir).await.is_ok() || !dir.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        });
    }
}

async fn remove(State(app): Shared, Path(id): Path<u64>) -> StatusCode {
    forget(&app, vec![id]);
    StatusCode::NO_CONTENT
}

async fn clear(State(app): Shared) -> StatusCode {
    let idle: Vec<u64> = app.items.lock().unwrap().values().filter(|it| !it.busy()).map(|it| it.id).collect();
    forget(&app, idle);
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct OpenQuery {
    #[serde(default)]
    reveal: bool,
}

/// Opens a result with its default app, or shows it selected in Explorer.
async fn open_output(State(app): Shared, Path((id, n)): Path<(u64, usize)>, Query(q): Query<OpenQuery>) -> Response {
    let path = app.items.lock().unwrap().get(&id).and_then(|it| it.outputs.get(n)).map(|o| o.path.clone());
    let Some(path) = path.filter(|p| p.exists()) else {
        return (StatusCode::NOT_FOUND, "Файл не найден").into_response();
    };
    let mut cmd = std::process::Command::new(if cfg!(windows) { "explorer" } else { "xdg-open" });
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if q.reveal {
            cmd.raw_arg(format!("/select,\"{}\"", path.display()));
        } else {
            cmd.raw_arg(format!("\"{}\"", path.display()));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = q.reveal;
        cmd.arg(path.parent().unwrap_or(&path));
    }
    let _ = cmd.spawn();
    StatusCode::NO_CONTENT.into_response()
}

async fn open_folder(State(app): Shared) -> StatusCode {
    let _ = tokio::fs::create_dir_all(&app.out_dir).await;
    let opener = if cfg!(windows) { "explorer" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(&app.out_dir).spawn();
    StatusCode::NO_CONTENT
}
