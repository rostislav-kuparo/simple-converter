//! FFmpeg discovery, hardware-encoder detection and media probing.

use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub const IMAGE_EXT: &[&str] = &[
    "jpg", "jpeg", "jpe", "jfif", "png", "webp", "bmp", "dib", "tif", "tiff", "gif", "ico", "avif",
    "heic", "heif", "jxl", "tga", "qoi", "psd", "ppm", "pgm", "pbm", "pnm", "exr", "hdr", "jp2",
];
pub const VIDEO_EXT: &[&str] = &[
    "mp4", "m4v", "mkv", "mov", "avi", "webm", "wmv", "flv", "ts", "mts", "m2ts", "mpg", "mpeg",
    "vob", "3gp", "3g2", "ogv", "mxf", "f4v", "asf", "divx", "rm", "rmvb", "y4m",
];
pub const AUDIO_EXT: &[&str] = &[
    "mp3", "wav", "flac", "aac", "m4a", "ogg", "oga", "opus", "wma", "aiff", "aif", "alac", "ac3",
    "eac3", "dts", "amr", "mka", "caf", "wv", "ape",
];

/// Spawns a child process without flashing a console window on Windows.
pub fn command(program: &Path) -> Command {
    let mut c = Command::new(program);
    c.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    c
}

#[derive(Clone, Debug)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks next to the executable first, then in PATH and common install locations.
    pub fn find() -> Option<Tools> {
        let exe = std::env::consts::EXE_SUFFIX;
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(d) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
            dirs.push(d.join("ffmpeg").join("bin"));
            dirs.push(d.join("ffmpeg"));
            dirs.push(d.join("bin"));
            dirs.push(d);
        }
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
        if cfg!(windows) {
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                dirs.push(PathBuf::from(local).join("Microsoft").join("WinGet").join("Links"));
            }
            dirs.push(PathBuf::from(r"C:\ffmpeg\bin"));
        }
        dirs.into_iter().find_map(|d| {
            let ffmpeg = d.join(format!("ffmpeg{exe}"));
            let ffprobe = d.join(format!("ffprobe{exe}"));
            (ffmpeg.is_file() && ffprobe.is_file()).then_some(Tools { ffmpeg, ffprobe })
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Caps {
    pub encoders: HashSet<String>,
    /// Working hardware encoders per codec family (already test-encoded once).
    pub hw_h264: Option<String>,
    pub hw_hevc: Option<String>,
    pub hw_av1: Option<String>,
}

impl Caps {
    pub fn has(&self, encoder: &str) -> bool {
        self.encoders.contains(encoder)
    }

    pub fn hw_name(&self) -> Option<&'static str> {
        let enc = self.hw_h264.as_deref().or(self.hw_hevc.as_deref())?;
        Some(if enc.ends_with("nvenc") {
            "NVENC"
        } else if enc.ends_with("qsv") {
            "Quick Sync"
        } else {
            "AMF"
        })
    }

    pub fn av1(&self) -> bool {
        self.hw_av1.is_some() || self.has("libsvtav1") || self.has("libaom-av1")
    }
}

pub async fn detect_caps(tools: Option<&Tools>) -> Caps {
    let Some(tools) = tools else { return Caps::default() };
    let Ok(out) = command(&tools.ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    else {
        return Caps::default();
    };

    let text = String::from_utf8_lossy(&out.stdout);
    let encoders: HashSet<String> = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("------"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect();

    // An encoder being compiled in says nothing about the GPU in this machine, so each
    // candidate gets a tiny real encode. All probes run in parallel.
    let families = [
        ("h264", ["h264_nvenc", "h264_qsv", "h264_amf"]),
        ("hevc", ["hevc_nvenc", "hevc_qsv", "hevc_amf"]),
        ("av1", ["av1_nvenc", "av1_qsv", "av1_amf"]),
    ];
    let known = &encoders;
    let probes = families.iter().flat_map(|(fam, list)| {
        list.iter()
            .filter(move |e| known.contains(**e))
            .map(move |e| async move { (*fam, *e, test_encoder(&tools.ffmpeg, e).await) })
    });
    let results = futures_util::future::join_all(probes).await;

    let pick = |fam: &str| {
        families
            .iter()
            .find(|(f, _)| *f == fam)
            .and_then(|(_, list)| {
                list.iter().find(|e| results.iter().any(|(f, n, ok)| *f == fam && n == *e && *ok))
            })
            .map(|e| e.to_string())
    };
    Caps { hw_h264: pick("h264"), hw_hevc: pick("hevc"), hw_av1: pick("av1"), encoders }
}

async fn test_encoder(ffmpeg: &Path, encoder: &str) -> bool {
    let run = command(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i", "color=black:s=256x256:r=30"])
        .args(["-frames:v", "3", "-c:v", encoder, "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    matches!(tokio::time::timeout(Duration::from_secs(10), run).await, Ok(Ok(s)) if s.success())
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Video,
    Image,
    Audio,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct MediaInfo {
    pub duration: f64,
    /// Display size (already swapped for rotated phone videos).
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub has_audio: bool,
    #[serde(skip)]
    pub ext: String,
    #[serde(skip)]
    pub video_index: Option<u64>,
    #[serde(skip)]
    pub acodecs: Vec<String>,
    #[serde(skip)]
    pub abitrate: Option<u64>,
    #[serde(skip)]
    pub pix_fmt: String,
    #[serde(skip)]
    pub size: u64,
}

pub async fn probe(tools: &Tools, file: &Path, ext: &str) -> Result<(Kind, MediaInfo), String> {
    let mut cmd = command(&tools.ffprobe);
    cmd.args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"]);
    if ext == "gif" {
        cmd.arg("-count_packets"); // tells animated GIFs apart from still ones
    } else if IMAGE_EXT.contains(&ext) {
        cmd.args(["-show_frames", "-read_intervals", "%+#1"]); // EXIF orientation lives on the frame
    }
    let out = cmd
        .arg(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("Не удалось запустить ffprobe: {e}"))?;
    let unreadable = || "Формат не поддерживается или файл повреждён".to_string();
    if !out.status.success() {
        return Err(unreadable());
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|_| unreadable())?;
    parse(&v, ext).ok_or_else(unreadable)
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn rate(v: &Value) -> Option<f64> {
    let (a, b) = v.as_str()?.split_once('/')?;
    let (a, b): (f64, f64) = (a.parse().ok()?, b.parse().ok()?);
    (a > 0.0 && b > 0.0).then(|| a / b)
}

fn parse(v: &Value, ext: &str) -> Option<(Kind, MediaInfo)> {
    let streams = v["streams"].as_array()?;
    let format = &v["format"];
    let mut info = MediaInfo {
        ext: ext.to_string(),
        duration: num(&format["duration"]).unwrap_or(0.0),
        size: num(&format["size"]).unwrap_or(0.0) as u64,
        ..Default::default()
    };

    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0);
    let mut frames = None;
    if let Some(s) = video {
        info.video_index = s["index"].as_u64();
        info.vcodec = s["codec_name"].as_str().map(str::to_string);
        info.width = s["width"].as_u64().unwrap_or(0) as u32;
        info.height = s["height"].as_u64().unwrap_or(0) as u32;
        info.pix_fmt = s["pix_fmt"].as_str().unwrap_or_default().to_string();
        info.fps = rate(&s["avg_frame_rate"]).or_else(|| rate(&s["r_frame_rate"])).unwrap_or(0.0);
        if info.duration <= 0.0 {
            info.duration = num(&s["duration"]).unwrap_or(0.0);
        }
        let side_rotation = |v: &Value| v["side_data_list"].as_array().and_then(|l| l.iter().find_map(|d| num(&d["rotation"])));
        let rotation = side_rotation(s)
            .or_else(|| num(&s["tags"]["rotate"]))
            .or_else(|| side_rotation(&v["frames"][0]))
            .unwrap_or(0.0);
        if (rotation.abs().round() as i64) % 180 == 90 {
            std::mem::swap(&mut info.width, &mut info.height);
        }
        frames = num(&s["nb_read_packets"]);
    }

    let audio: Vec<&Value> = streams.iter().filter(|s| s["codec_type"] == "audio").collect();
    info.has_audio = !audio.is_empty();
    info.acodecs = audio.iter().filter_map(|s| s["codec_name"].as_str().map(str::to_string)).collect();
    info.acodec = info.acodecs.first().cloned();
    info.abitrate = audio.first().and_then(|s| num(&s["bit_rate"])).map(|b| b as u64);

    let kind = if IMAGE_EXT.contains(&ext) {
        video?;
        if ext == "gif" && frames.unwrap_or(1.0) > 1.0 { Kind::Video } else { Kind::Image }
    } else if VIDEO_EXT.contains(&ext) || AUDIO_EXT.contains(&ext) {
        match (video.is_some() && !AUDIO_EXT.contains(&ext), info.has_audio) {
            (true, _) => Kind::Video,
            (false, true) => Kind::Audio,
            (false, false) => return None,
        }
    } else if video.is_some() {
        let fmt = format["format_name"].as_str().unwrap_or_default();
        if fmt.contains("_pipe") || fmt.starts_with("image2") || info.duration < 0.05 {
            Kind::Image
        } else {
            Kind::Video
        }
    } else if info.has_audio {
        Kind::Audio
    } else {
        return None;
    };
    Some((kind, info))
}
