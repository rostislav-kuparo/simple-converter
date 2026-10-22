//! Runs one conversion: queueing, the FFmpeg process, progress and cancellation.

use crate::media;
use crate::plan::{self, Plan, Settings};
use crate::state::{App, OutFile, Status, split_name};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::{OwnedSemaphorePermit, oneshot};
use tokio_util::sync::CancellationToken;

type Turn = oneshot::Receiver<OwnedSemaphorePermit>;

enum Failure {
    Cancelled,
    Gone,
    Error(String),
}

impl From<String> for Failure {
    fn from(e: String) -> Self {
        Failure::Error(e)
    }
}

/// Queues the items in the given order; busy ones are left alone.
pub async fn start(app: &Arc<App>, ids: &[u64], settings: &Settings) {
    let caps = app.caps().await;
    for &id in ids {
        let token = CancellationToken::new();
        let queued = app.update(id, |it| {
            if it.busy() {
                return None;
            }
            it.status = Status::Queued;
            it.progress = 0.0;
            it.speed = None;
            it.outputs.clear();
            it.error = None;
            it.note = None;
            it.cancel = token.clone();
            Some((it.kind, it.info.clone()))
        });
        let Some(Some((kind, info))) = queued else { continue };
        // Only the kind of slot matters here; the job builds its plan again when its turn comes.
        let heavy = plan::build(kind, &info, settings, caps, true).is_ok_and(|p| p.heavy);
        let turn = app.take_turn(heavy);
        app.active_jobs.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(run(app.clone(), id, settings.clone(), token, turn));
    }
}

async fn run(app: Arc<App>, id: u64, settings: Settings, cancel: CancellationToken, turn: Turn) {
    let result = convert(&app, id, &settings, &cancel, turn).await;
    app.update(id, |it| {
        it.speed = None;
        match result {
            Ok((outputs, note)) => {
                it.status = Status::Done;
                it.progress = 1.0;
                it.outputs = outputs;
                it.note = note;
            }
            Err(Failure::Cancelled) => it.status = Status::Cancelled,
            Err(Failure::Error(e)) => {
                it.status = Status::Error;
                it.error = Some(e);
            }
            Err(Failure::Gone) => {}
        }
    });
    app.active_jobs.fetch_sub(1, Ordering::SeqCst);
    app.pump(); // our slot is free again
}

async fn convert(
    app: &Arc<App>,
    id: u64,
    settings: &Settings,
    cancel: &CancellationToken,
    turn: Turn,
) -> Result<(Vec<OutFile>, Option<String>), Failure> {
    let (kind, info, src, name, src_size) = {
        let items = app.items.lock().unwrap();
        let it = items.get(&id).ok_or(Failure::Gone)?;
        (it.kind, it.info.clone(), it.src.clone(), it.name.clone(), it.size)
    };
    let tools = app.tools.clone().ok_or("FFmpeg не найден".to_string())?;
    let caps = app.caps().await;
    let mut plan = plan::build(kind, &info, settings, caps, true)?;

    let _slot = tokio::select! {
        slot = turn => slot.map_err(|_| Failure::Gone)?,
        _ = cancel.cancelled() => return Err(Failure::Cancelled),
    };
    app.update(id, |it| {
        it.status = Status::Running;
        it.progress = 0.0;
    });

    tokio::fs::create_dir_all(&app.out_dir)
        .await
        .map_err(|e| format!("Не удалось создать папку {}: {e}", app.out_dir.display()))?;
    let (stem, _) = split_name(&name);
    let paths: Vec<PathBuf> = plan.outputs.iter().map(|o| app.reserve_output(&stem, &o.ext)).collect();

    let mut result = execute(app, id, &tools.ffmpeg, &src, &plan, &paths, info.duration, cancel).await;
    if plan.gpu && matches!(result, Err(Failure::Error(_))) {
        // Hardware encoders can refuse odd sizes or formats; the CPU path always works.
        remove_files(&paths).await;
        plan = plan::build(kind, &info, settings, caps, false)?;
        app.update(id, |it| it.progress = 0.0);
        result = execute(app, id, &tools.ffmpeg, &src, &plan, &paths, info.duration, cancel).await;
        plan.note = Some(plan.note.map_or("через CPU".into(), |n| format!("{n}, через CPU")));
    }

    let outcome = match result {
        Ok(()) => {
            let mut outputs = Vec::new();
            for (spec, path) in plan.outputs.iter().zip(&paths) {
                let size = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
                let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                outputs.push(OutFile { name, size, kind: spec.kind, path: path.clone() });
            }
            let grew = outputs.iter().any(|o| o.kind == kind && o.size > src_size);
            if settings.mode == "compress" && grew {
                plan.note = Some("исходник уже был сжат сильнее — попробуйте «Компактно», H.265 или меньшее разрешение".into());
            }
            Ok((outputs, plan.note))
        }
        Err(e) => {
            remove_files(&paths).await;
            Err(e)
        }
    };
    app.release_outputs(&paths);
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    app: &Arc<App>,
    id: u64,
    ffmpeg: &Path,
    src: &Path,
    plan: &Plan,
    paths: &[PathBuf],
    duration: f64,
    cancel: &CancellationToken,
) -> Result<(), Failure> {
    let mut cmd = media::command(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(["-progress", "pipe:1", "-nostats", "-stats_period", "0.25"])
        .args(&plan.input_args)
        .arg("-i")
        .arg(src);
    for (spec, path) in plan.outputs.iter().zip(paths) {
        cmd.args(&spec.args).arg(path);
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Не удалось запустить FFmpeg: {e}"))?;

    let mut stderr = child.stderr.take().unwrap();
    let errors = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).into_owned()
    });

    let stdout = child.stdout.take().unwrap();
    let progress = async {
        let mut lines = BufReader::new(stdout).split(b'\n');
        let mut position = 0.0;
        let mut speed = None;
        let mut last = Instant::now() - Duration::from_secs(1);
        while let Ok(Some(line)) = lines.next_segment().await {
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            if let Some(v) = line.strip_prefix("out_time_us=") {
                position = v.parse::<f64>().map(|us| us / 1e6).unwrap_or(position);
            } else if let Some(v) = line.strip_prefix("speed=") {
                speed = v.trim().strip_suffix('x').and_then(|x| x.trim().parse::<f64>().ok());
            } else if line.starts_with("progress=") && duration > 0.0 && last.elapsed() >= Duration::from_millis(200) {
                last = Instant::now();
                let p = (position / duration).clamp(0.0, 0.99);
                let speed = speed.filter(|s| *s > 0.0).map(|s| format!("{s:.1}×"));
                app.update(id, |it| {
                    it.progress = p;
                    it.speed = speed;
                });
            }
        }
    };

    tokio::select! {
        _ = progress => {}
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(Failure::Cancelled);
        }
    }
    let status = tokio::select! {
        status = child.wait() => status.map_err(|e| e.to_string())?,
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            return Err(Failure::Cancelled);
        }
    };
    let log = errors.await.unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(Failure::Error(summarize(&log)))
    }
}

fn summarize(log: &str) -> String {
    let lines: Vec<&str> = log.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return "FFmpeg завершился с ошибкой".into();
    }
    let tail = lines[lines.len().saturating_sub(3)..].join(" · ");
    if tail.chars().count() > 400 { tail.chars().take(400).collect::<String>() + "…" } else { tail }
}

async fn remove_files(paths: &[PathBuf]) {
    for p in paths {
        let _ = tokio::fs::remove_file(p).await;
    }
}
