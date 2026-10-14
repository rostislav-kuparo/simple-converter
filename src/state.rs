//! Shared application state: the file list, live events and output-name bookkeeping.

use crate::media::{self, Caps, Kind, MediaInfo, Tools};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use std::time::Instant;
use tokio::sync::{OnceCell, Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Queued,
    Running,
    Done,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutFile {
    pub name: String,
    pub size: u64,
    pub kind: Kind,
    #[serde(skip)]
    pub path: PathBuf,
}

pub struct Item {
    pub id: u64,
    /// Client-side upload key, lets the page match its placeholder row.
    pub key: String,
    pub name: String,
    pub size: u64,
    pub kind: Kind,
    pub info: MediaInfo,
    pub src: PathBuf,
    pub status: Status,
    pub progress: f64,
    pub speed: Option<String>,
    pub outputs: Vec<OutFile>,
    pub error: Option<String>,
    pub note: Option<String>,
    pub cancel: CancellationToken,
}

impl Item {
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "key": self.key,
            "name": self.name,
            "size": self.size,
            "kind": self.kind,
            "info": self.info,
            "status": self.status,
            "progress": self.progress,
            "speed": self.speed,
            "outputs": self.outputs,
            "error": self.error,
            "note": self.note,
        })
    }

    pub fn busy(&self) -> bool {
        matches!(self.status, Status::Queued | Status::Running)
    }
}

pub struct App {
    pub items: Mutex<HashMap<u64, Item>>,
    pub next_id: AtomicU64,
    pub events: broadcast::Sender<String>,
    pub tools: Option<Tools>,
    caps: OnceCell<Caps>,
    pub out_dir: PathBuf,
    pub tmp_dir: PathBuf,
    /// Video encodes (each already uses every core or the GPU).
    pub heavy: Semaphore,
    /// Images, audio and stream copies.
    pub light: Semaphore,
    reserved: Mutex<HashSet<PathBuf>>,
    pub clients: AtomicUsize,
    pub active_jobs: AtomicUsize,
    pub ever_connected: AtomicBool,
    pub last_seen: Mutex<Instant>,
}

impl App {
    pub fn new(tools: Option<Tools>, out_dir: PathBuf, tmp_dir: PathBuf) -> App {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        App {
            items: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            events: broadcast::channel(1024).0,
            tools,
            caps: OnceCell::new(),
            out_dir,
            tmp_dir,
            heavy: Semaphore::new(2),
            light: Semaphore::new((cores / 2).max(2)),
            reserved: Mutex::new(HashSet::new()),
            clients: AtomicUsize::new(0),
            active_jobs: AtomicUsize::new(0),
            ever_connected: AtomicBool::new(false),
            last_seen: Mutex::new(Instant::now()),
        }
    }

    pub async fn caps(&self) -> &Caps {
        self.caps.get_or_init(|| media::detect_caps(self.tools.as_ref())).await
    }

    pub fn emit(&self, event: Value) {
        let _ = self.events.send(event.to_string());
    }

    /// Mutates an item and pushes its new state to every open page.
    pub fn update<R>(&self, id: u64, f: impl FnOnce(&mut Item) -> R) -> Option<R> {
        let (r, view) = {
            let mut items = self.items.lock().unwrap();
            let item = items.get_mut(&id)?;
            let r = f(item);
            (r, item.view())
        };
        self.emit(json!({ "type": "item", "item": view }));
        Some(r)
    }

    /// Picks a free "name.ext" / "name (2).ext" in the output folder and holds it until released.
    pub fn reserve_output(&self, stem: &str, ext: &str) -> PathBuf {
        let mut reserved = self.reserved.lock().unwrap();
        let path = (1..)
            .map(|i| match i {
                1 => self.out_dir.join(format!("{stem}.{ext}")),
                n => self.out_dir.join(format!("{stem} ({n}).{ext}")),
            })
            .find(|p| !p.exists() && !reserved.contains(p))
            .unwrap();
        reserved.insert(path.clone());
        path
    }

    pub fn release_outputs(&self, paths: &[PathBuf]) {
        let mut reserved = self.reserved.lock().unwrap();
        for p in paths {
            reserved.remove(p);
        }
    }
}

/// Makes a user-supplied file name safe for Windows paths.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_end_matches('.').to_string();
    if clean.is_empty() { "file".into() } else { clean }
}

pub fn split_name(name: &str) -> (String, String) {
    let path = Path::new(name);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    let mut stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file").to_string();
    if stem.chars().count() > 120 {
        stem = stem.chars().take(120).collect();
    }
    (stem, ext)
}
