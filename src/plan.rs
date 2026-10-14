//! Turns user settings + probed media info into FFmpeg arguments.

use crate::media::{Caps, Kind, MediaInfo};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// "convert" or "compress".
    pub mode: String,
    /// Container for videos in convert mode: mp4 mkv mov webm avi gif.
    pub video_format: String,
    /// Compress mode: h264 hevc av1.
    pub codec: String,
    /// Compress mode: high balanced small.
    pub quality: String,
    /// Compress mode: cap for the short side, 0 = keep.
    pub resolution: u32,
    /// Compress mode: target file size in MB, 0 = off.
    pub target_mb: f64,
    pub gpu: bool,
    /// same jpg png webp avif jxl gif bmp tiff ico.
    pub image_format: String,
    pub image_quality: u32,
    /// Longest side cap for images, 0 = keep.
    pub image_max: u32,
    /// Target for audio files: mp3 aac wav flac opus ogg.
    pub audio_format: String,
    pub extract_audio: bool,
    /// Format of the separated track: copy mp3 aac wav flac opus ogg.
    pub extract_format: String,
    /// What happens to the video when audio is separated: mute keep none.
    pub video_audio: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mode: "convert".into(),
            video_format: "mp4".into(),
            codec: "h264".into(),
            quality: "balanced".into(),
            resolution: 0,
            target_mb: 0.0,
            gpu: true,
            image_format: "jpg".into(),
            image_quality: 82,
            image_max: 0,
            audio_format: "mp3".into(),
            extract_audio: false,
            extract_format: "mp3".into(),
            video_audio: "mute".into(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Plain conversion: keep the picture visually identical.
    Max,
    High,
    Balanced,
    Small,
}

impl Level {
    fn of(s: &str) -> Level {
        match s {
            "high" => Level::High,
            "small" => Level::Small,
            _ => Level::Balanced,
        }
    }

    fn pick<T>(self, max: T, high: T, balanced: T, small: T) -> T {
        match self {
            Level::Max => max,
            Level::High => high,
            Level::Balanced => balanced,
            Level::Small => small,
        }
    }
}

pub struct OutSpec {
    pub kind: Kind,
    pub ext: String,
    pub args: Vec<String>,
}

#[derive(Default)]
pub struct Plan {
    pub input_args: Vec<String>,
    pub outputs: Vec<OutSpec>,
    /// Real video encoding (as opposed to images, audio or a stream copy).
    pub heavy: bool,
    pub gpu: bool,
    pub note: Option<String>,
}

pub fn build(kind: Kind, info: &MediaInfo, s: &Settings, caps: &Caps, allow_gpu: bool) -> Result<Plan, String> {
    match kind {
        Kind::Video => video(info, s, caps, allow_gpu && s.gpu),
        Kind::Image => image(info, s, caps),
        Kind::Audio => audio_file(s),
    }
}

trait Push {
    fn push_all<const N: usize>(&mut self, items: [&str; N]);
}

impl Push for Vec<String> {
    fn push_all<const N: usize>(&mut self, items: [&str; N]) {
        self.extend(items.iter().map(|s| s.to_string()));
    }
}

fn video(info: &MediaInfo, s: &Settings, caps: &Caps, gpu: bool) -> Result<Plan, String> {
    let compress = s.mode == "compress";
    let level = if compress { Level::of(&s.quality) } else { Level::Max };
    let audio_level = if compress { level } else { Level::High };
    let keep_video = !(s.extract_audio && s.video_audio == "none");
    let extract = s.extract_audio && info.has_audio;
    let video_sound = info.has_audio && !(s.extract_audio && s.video_audio != "keep");
    let mut plan = Plan::default();

    if !keep_video && !info.has_audio {
        return Err("В файле нет звуковой дорожки".into());
    }

    if keep_video {
        let index = info.video_index.ok_or("В файле нет видеодорожки")?;
        let container = if compress { "mp4" } else { container_of(&s.video_format) };
        let mut a = vec!["-map".to_string(), format!("0:{index}")];

        if container == "gif" {
            let fps = if info.fps > 0.0 { info.fps.min(15.0) } else { 15.0 };
            let vf = format!(
                "fps={fps:.3},{},split[ga][gb];[ga]palettegen=stats_mode=diff[gp];[gb][gp]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle",
                scale_short(480, false)
            );
            a.push_all(["-vf", &vf, "-an", "-sn", "-loop", "0"]);
            plan.heavy = true;
            plan.outputs.push(OutSpec { kind: Kind::Video, ext: "gif".into(), args: a });
        } else {
            let src = info.vcodec.as_deref().unwrap_or_default();
            let copy_video = !compress && info.ext != "gif" && video_copy_ok(container, src);

            // Bitrate budget when the user asked for a maximum file size. A file that already
            // fits is compressed by quality instead: a generous budget would only inflate it.
            let fits = info.size > 0 && (info.size as f64) < s.target_mb * 1e6;
            let budget = (compress && s.target_mb > 0.0 && info.duration > 0.5 && !fits).then(|| {
                let total = s.target_mb * 8000.0 / info.duration; // kbit/s, 1 MB = 10^6 bytes
                let audio = if !video_sound { 0.0 } else if total < 400.0 { 48.0 } else if total < 1200.0 { 96.0 } else { 128.0 };
                ((total * 0.95 - audio).max(40.0) as u32, audio as u32)
            });

            if copy_video {
                a.push_all(["-c:v", "copy"]);
                if src == "hevc" && matches!(container, "mp4" | "mov") {
                    a.push_all(["-tag:v", "hvc1"]);
                }
            } else {
                let mut codec = if compress { s.codec.as_str() } else { default_codec(container, caps) };
                if codec == "av1" && !caps.av1() {
                    codec = "h264";
                }
                let short = info.width.min(info.height);
                let mut res = if compress { s.resolution } else { 0 };
                if res == 0 && let Some((vkbps, _)) = budget {
                    res = auto_resolution(info, vkbps, codec);
                }
                if res > 0 && short > 0 && res < short {
                    a.push_all(["-vf", &scale_short(res, true)]);
                } else if info.width % 2 == 1 || info.height % 2 == 1 {
                    a.push_all(["-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2"]);
                }
                let used_gpu = video_encoder(&mut a, codec, level, budget.map(|b| b.0), caps, gpu, info, container);
                plan.heavy = true;
                plan.gpu = used_gpu;
                if used_gpu {
                    plan.input_args.push_all(["-hwaccel", "auto"]);
                }
            }

            if video_sound {
                let maps: [&str; 2] = if container == "avi" || budget.is_some() { ["-map", "0:a:0"] } else { ["-map", "0:a?"] };
                a.push_all(maps);
                let compatible = info.acodecs.iter().all(|c| audio_copy_ok(container, c));
                let copy_audio = match budget {
                    Some(_) => false,
                    None if compress => {
                        let limit = audio_level.pick(256, 192, 128, 96) * 1000 * 115 / 100;
                        compatible
                            && matches!(info.acodec.as_deref(), Some("aac" | "mp3" | "opus"))
                            && info.abitrate.is_some_and(|b| b <= limit)
                    }
                    None => compatible,
                };
                if copy_audio {
                    a.push_all(["-c:a", "copy"]);
                    if copy_video {
                        plan.note = Some("без перекодирования".into());
                    }
                } else {
                    let kbps = match budget {
                        Some((_, akbps)) => akbps,
                        None => audio_level.pick(256, 192, 128, 96),
                    };
                    let kbps = format!("{kbps}k");
                    match container {
                        "webm" => a.push_all(["-c:a", "libopus", "-b:a", &kbps]),
                        "avi" => a.push_all(["-c:a", "libmp3lame", "-b:a", &kbps]),
                        _ => a.push_all(["-c:a", "aac", "-b:a", &kbps]),
                    }
                }
            } else {
                a.push("-an".into());
                if copy_video {
                    plan.note = Some("без перекодирования".into());
                }
            }

            if container == "mkv" && info.ext == "mkv" {
                a.push_all(["-map", "0:s?", "-c:s", "copy"]);
            } else {
                a.push("-sn".into());
            }
            if matches!(container, "mp4" | "mov") {
                a.push_all(["-movflags", "+faststart"]);
            }
            a.push_all(["-max_muxing_queue_size", "4096"]);
            plan.outputs.push(OutSpec { kind: Kind::Video, ext: container.into(), args: a });
        }
    }

    if extract {
        let mut a = Vec::new();
        a.push_all(["-map", "0:a:0", "-vn", "-sn"]);
        let ext = audio_encoder(&mut a, &s.extract_format, audio_level, info.acodec.as_deref());
        plan.outputs.push(OutSpec { kind: Kind::Audio, ext, args: a });
    }
    Ok(plan)
}

fn container_of(format: &str) -> &'static str {
    match format {
        "mkv" => "mkv",
        "mov" => "mov",
        "webm" => "webm",
        "avi" => "avi",
        "gif" => "gif",
        _ => "mp4",
    }
}

fn default_codec(container: &str, caps: &Caps) -> &'static str {
    match container {
        "webm" if caps.av1() => "av1",
        "webm" => "vp9",
        "avi" => "mpeg4",
        _ => "h264",
    }
}

fn video_copy_ok(container: &str, codec: &str) -> bool {
    match container {
        "mp4" => matches!(codec, "h264" | "hevc" | "av1" | "mpeg4" | "vp9"),
        "mov" => matches!(codec, "h264" | "hevc" | "mpeg4" | "prores" | "mjpeg"),
        "webm" => matches!(codec, "vp8" | "vp9" | "av1"),
        "avi" => matches!(codec, "mpeg4" | "h264" | "mjpeg" | "msmpeg4v2" | "msmpeg4v3"),
        "mkv" => !matches!(codec, "gif" | "apng" | ""),
        _ => false,
    }
}

fn audio_copy_ok(container: &str, codec: &str) -> bool {
    match container {
        "mp4" => matches!(codec, "aac" | "mp3" | "ac3" | "eac3" | "alac" | "opus" | "flac"),
        "mov" => matches!(codec, "aac" | "mp3" | "ac3" | "eac3" | "alac" | "pcm_s16le" | "pcm_s24le" | "pcm_s16be" | "pcm_s24be"),
        "webm" => matches!(codec, "opus" | "vorbis"),
        "avi" => matches!(codec, "mp3" | "ac3" | "pcm_s16le"),
        "mkv" => true,
        _ => false,
    }
}

/// Limits the short side to `r` px, keeping orientation and aspect ratio.
fn scale_short(r: u32, even: bool) -> String {
    let auto = if even { -2 } else { -1 };
    format!("scale=w='if(gte(iw,ih),{auto},min({r},iw))':h='if(gte(iw,ih),min({r},ih),{auto})':flags=lanczos")
}

/// When squeezing into a size limit, a smaller frame looks far better than a starved one.
fn auto_resolution(info: &MediaInfo, vkbps: u32, codec: &str) -> u32 {
    let (w, h) = (info.width as f64, info.height as f64);
    if w <= 0.0 || h <= 0.0 {
        return 0;
    }
    let fps = if info.fps > 0.0 { info.fps.min(60.0) } else { 30.0 };
    let min_bpp = match codec {
        "hevc" => 0.035,
        "av1" => 0.03,
        _ => 0.05,
    };
    let bps = vkbps as f64 * 1000.0;
    if bps / (w * h * fps) >= min_bpp {
        return 0;
    }
    let (short, long) = (w.min(h), w.max(h));
    let fit = (bps / (fps * min_bpp) * short / long).sqrt();
    [1440, 1080, 720, 540, 480, 360, 240]
        .into_iter()
        .find(|&step| (step as f64) <= fit && (step as f64) < short)
        .unwrap_or(if short > 240.0 { 240 } else { 0 })
}

/// Appends encoder options; returns true if a hardware encoder was chosen.
#[allow(clippy::too_many_arguments)]
fn video_encoder(
    a: &mut Vec<String>,
    codec: &str,
    level: Level,
    kbps: Option<u32>,
    caps: &Caps,
    gpu: bool,
    info: &MediaInfo,
    container: &str,
) -> bool {
    let ten_bit = codec != "h264" && (info.pix_fmt.contains("10") || info.pix_fmt.contains("12"));
    let hw = if gpu {
        match codec {
            "h264" => caps.hw_h264.as_deref(),
            "hevc" => caps.hw_hevc.as_deref(),
            "av1" => caps.hw_av1.as_deref(),
            _ => None,
        }
    } else {
        None
    };
    let rate = |a: &mut Vec<String>, k: u32| {
        a.push_all(["-b:v", &format!("{k}k"), "-maxrate", &format!("{}k", k * 3 / 2), "-bufsize", &format!("{}k", k * 2)]);
    };
    let hvc1 = codec == "hevc" && matches!(container, "mp4" | "mov");

    if let Some(enc) = hw {
        let q = match codec {
            "hevc" => level.pick(21, 24, 28, 33),
            "av1" => level.pick(24, 28, 33, 38),
            _ => level.pick(19, 22, 26, 31),
        }
        .to_string();
        a.push_all(["-c:v", enc]);
        if enc.ends_with("_nvenc") {
            a.push_all(["-preset", "p5", "-tune", "hq", "-spatial_aq", "1", "-rc-lookahead", "20"]);
            match kbps {
                Some(k) => {
                    a.push_all(["-rc", "vbr", "-multipass", "qres"]);
                    rate(a, k);
                }
                None => a.push_all(["-rc", "vbr", "-cq", &q, "-b:v", "0"]),
            }
            a.push_all(["-pix_fmt", if ten_bit { "p010le" } else { "yuv420p" }]);
        } else if enc.ends_with("_qsv") {
            a.push_all(["-preset", "medium"]);
            match kbps {
                Some(k) => rate(a, k),
                None => a.push_all(["-global_quality", &q]),
            }
            a.push_all(["-pix_fmt", if ten_bit { "p010le" } else { "nv12" }]);
        } else {
            a.push_all(["-quality", "balanced"]);
            match kbps {
                Some(k) => {
                    a.push_all(["-rc", "vbr_peak"]);
                    rate(a, k);
                }
                None => a.push_all(["-rc", "cqp", "-qp_i", &q, "-qp_p", &q]),
            }
            a.push_all(["-pix_fmt", if ten_bit { "p010le" } else { "nv12" }]);
        }
        if hvc1 {
            a.push_all(["-tag:v", "hvc1"]);
        }
        return true;
    }

    let pix = if ten_bit { "yuv420p10le" } else { "yuv420p" };
    match codec {
        "hevc" => {
            a.push_all(["-c:v", "libx265", "-preset", "fast", "-x265-params", "log-level=error"]);
            match kbps {
                Some(k) => rate(a, k),
                None => a.push_all(["-crf", &level.pick(20, 23, 26, 30).to_string()]),
            }
            a.push_all(["-pix_fmt", pix]);
        }
        "av1" if caps.has("libsvtav1") => {
            a.push_all(["-c:v", "libsvtav1", "-preset", "8"]);
            match kbps {
                Some(k) => a.push_all(["-b:v", &format!("{k}k")]),
                None => a.push_all(["-crf", &level.pick(26, 30, 35, 42).to_string()]),
            }
            a.push_all(["-pix_fmt", pix]);
        }
        "av1" => {
            a.push_all(["-c:v", "libaom-av1", "-cpu-used", "6", "-row-mt", "1"]);
            match kbps {
                Some(k) => a.push_all(["-b:v", &format!("{k}k")]),
                None => a.push_all(["-crf", &level.pick(26, 30, 35, 42).to_string(), "-b:v", "0"]),
            }
            a.push_all(["-pix_fmt", pix]);
        }
        "vp9" => {
            a.push_all(["-c:v", "libvpx-vp9", "-deadline", "good", "-cpu-used", "4", "-row-mt", "1"]);
            match kbps {
                Some(k) => a.push_all(["-b:v", &format!("{k}k")]),
                None => a.push_all(["-crf", &level.pick(30, 33, 36, 42).to_string(), "-b:v", "0"]),
            }
            a.push_all(["-pix_fmt", "yuv420p"]);
        }
        "mpeg4" => {
            a.push_all(["-c:v", "mpeg4", "-vtag", "XVID"]);
            match kbps {
                Some(k) => rate(a, k),
                None => a.push_all(["-q:v", &level.pick(2, 3, 5, 8).to_string()]),
            }
            a.push_all(["-pix_fmt", "yuv420p"]);
        }
        _ => {
            let preset = if level == Level::Max { "fast" } else { "medium" };
            a.push_all(["-c:v", "libx264", "-preset", preset]);
            match kbps {
                Some(k) => rate(a, k),
                None => a.push_all(["-crf", &level.pick(18, 20, 23, 28).to_string()]),
            }
            a.push_all(["-pix_fmt", "yuv420p"]);
        }
    }
    if hvc1 {
        a.push_all(["-tag:v", "hvc1"]);
    }
    false
}

/// Appends audio encoder options and returns the file extension.
fn audio_encoder(a: &mut Vec<String>, format: &str, level: Level, source: Option<&str>) -> String {
    let ext = match format {
        "copy" => {
            a.push_all(["-c:a", "copy"]);
            return match source.unwrap_or_default() {
                "aac" | "alac" => "m4a",
                "mp3" => "mp3",
                "opus" => "opus",
                "vorbis" => "ogg",
                "flac" => "flac",
                "ac3" => "ac3",
                "eac3" => "eac3",
                "dts" => "dts",
                "pcm_s16le" | "pcm_s24le" | "pcm_s32le" | "pcm_f32le" | "pcm_u8" => "wav",
                _ => "mka",
            }
            .into();
        }
        "aac" => {
            a.push_all(["-c:a", "aac", "-b:a", &format!("{}k", level.pick(256, 192, 128, 96))]);
            "m4a"
        }
        "opus" => {
            a.push_all(["-c:a", "libopus", "-b:a", &format!("{}k", level.pick(192, 160, 112, 80))]);
            "opus"
        }
        "ogg" => {
            a.push_all(["-c:a", "libvorbis", "-q:a", &level.pick(8, 6, 4, 2).to_string()]);
            "ogg"
        }
        "flac" => {
            a.push_all(["-c:a", "flac"]);
            "flac"
        }
        "wav" => {
            a.push_all(["-c:a", "pcm_s16le"]);
            "wav"
        }
        _ => {
            a.push_all(["-c:a", "libmp3lame", "-q:a", &level.pick(0, 2, 4, 6).to_string()]);
            "mp3"
        }
    };
    ext.into()
}

fn audio_file(s: &Settings) -> Result<Plan, String> {
    let level = if s.mode == "compress" { Level::of(&s.quality) } else { Level::High };
    let mut a = Vec::new();
    a.push_all(["-map", "0:a:0", "-vn", "-sn"]);
    let ext = audio_encoder(&mut a, &s.audio_format, level, None);
    Ok(Plan { outputs: vec![OutSpec { kind: Kind::Audio, ext, args: a }], ..Default::default() })
}

fn same_image_format(ext: &str) -> &'static str {
    match ext {
        "png" => "png",
        "webp" => "webp",
        "avif" => "avif",
        "jxl" => "jxl",
        "gif" => "gif",
        "bmp" | "dib" => "bmp",
        "tif" | "tiff" => "tiff",
        "ico" => "ico",
        _ => "jpg",
    }
}

fn image(info: &MediaInfo, s: &Settings, caps: &Caps) -> Result<Plan, String> {
    let format = match s.image_format.as_str() {
        "" | "same" => same_image_format(&info.ext),
        f => same_image_format(f),
    };
    let q = s.image_quality.clamp(1, 100);
    let fit = |m: u32| format!("scale=w='min(iw,{m})':h='min(ih,{m})':force_original_aspect_ratio=decrease:flags=lanczos");

    let mut chain: Vec<String> = Vec::new();
    if format == "ico" {
        chain.push(fit(256));
    } else if s.mode == "compress" && s.image_max > 0 && info.width.max(info.height) > s.image_max {
        chain.push(fit(s.image_max));
    }
    let pf = info.pix_fmt.as_str();
    let alpha = pf == "pal8"
        || ["yuva", "ya", "gbrap"].iter().any(|p| pf.starts_with(p))
        || ["rgba", "bgra", "argb", "abgr"].iter().any(|p| pf.contains(p));
    if alpha && matches!(format, "jpg" | "bmp" | "avif") {
        // These formats have no transparency: put the picture on white instead of black.
        chain.push("format=rgba,split[fa][fb];[fa]drawbox=c=white:replace=1:t=fill[fbg];[fbg][fb]overlay=format=auto".into());
    }
    if format == "gif" {
        chain.push("split[ga][gb];[ga]palettegen=reserve_transparent=1[gp];[gb][gp]paletteuse=alpha_threshold=128".into());
    }

    let mut a = Vec::new();
    if !chain.is_empty() {
        a.push_all(["-vf", &chain.join(",")]);
    }
    a.push_all(["-frames:v", "1"]);
    match format {
        "png" if s.mode == "compress" => a.push_all(["-c:v", "png", "-compression_level", "9", "-pred", "mixed"]),
        "png" => a.push_all(["-c:v", "png"]),
        "webp" => a.push_all(["-c:v", "libwebp", "-quality", &q.to_string(), "-compression_level", "4"]),
        "avif" if caps.has("libaom-av1") => {
            let crf = (12.0 + (100 - q) as f64 * 0.5).round().to_string();
            a.push_all(["-c:v", "libaom-av1", "-usage", "allintra", "-still-picture", "1", "-crf", &crf]);
            a.push_all(["-cpu-used", "6", "-row-mt", "1", "-pix_fmt", "yuv420p"]);
        }
        "avif" => {
            let crf = (12.0 + (100 - q) as f64 * 0.5).round().to_string();
            a.push_all(["-c:v", "libsvtav1", "-crf", &crf, "-preset", "6", "-pix_fmt", "yuv420p"]);
        }
        "jxl" => a.push_all(["-c:v", "libjxl", "-distance", &format!("{:.1}", (100 - q) as f64 / 10.0)]),
        "gif" => a.push_all(["-c:v", "gif"]),
        "bmp" => a.push_all(["-c:v", "bmp", "-pix_fmt", "bgr24"]),
        "tiff" => a.push_all(["-c:v", "tiff", "-compression_algo", "lzw"]),
        "ico" => a.push_all(["-c:v", "png", "-pix_fmt", "rgba"]),
        _ => {
            let qv = (2.0 + (100 - q) as f64 * 0.18).round().clamp(2.0, 31.0).to_string();
            a.push_all(["-c:v", "mjpeg", "-q:v", &qv, "-pix_fmt", "yuvj420p"]);
        }
    }
    if matches!(format, "jpg" | "png" | "bmp" | "tiff" | "jxl") {
        a.push_all(["-update", "1"]); // a literal file name, even if it contains '%'
    }
    Ok(Plan { outputs: vec![OutSpec { kind: Kind::Image, ext: format.into(), args: a }], ..Default::default() })
}
