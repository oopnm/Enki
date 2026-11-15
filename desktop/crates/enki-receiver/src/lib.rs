//! Receives the Enki Link camera stream from the phone and feeds it to a V4L2 loopback device.
//!
//! Flow: `adb forward` → connect to the phone app → Enki Link handshake → H.264 frames are piped into
//! ffmpeg, which decodes them and writes to `/dev/videoN`. The session reconnects by itself when the
//! phone app is stopped and started again, and runs until [`run`]'s stop signal fires.
//!
//! Everything is reported through [`Event`]s, so any UI (GTK today) can show the state.

use enki_protocol::{Channel, FrameHeader, Kind, Preamble, FLAG_DISCONTINUITY, FRAME_HEADER_LEN, PREAMBLE_LEN};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_PORT: u16 = 28200;
const RETRY_DELAY: Duration = Duration::from_millis(1500);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Config {
    /// Target V4L2 device, e.g. `/dev/video10`.
    pub device: String,
    pub port: u16,
    /// Frame rate the decoder assumes (the stream carries no reliable rate in the H.264 headers).
    pub fps: u32,
    pub adb: String,
    pub ffmpeg: String,
    /// Run `adb forward` before connecting. Tests turn this off.
    pub setup_adb: bool,
    /// Replaces the ffmpeg command (program, args). Tests use it to capture the stream.
    pub sink_override: Option<(String, Vec<String>)>,
}

impl Config {
    pub fn new(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            port: DEFAULT_PORT,
            fps: 30,
            adb: "adb".into(),
            ffmpeg: "ffmpeg".into(),
            setup_adb: true,
            sink_override: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for the phone app to accept a connection.
    Connecting,
    Streaming,
    /// The receiver has ended (after stop, or a fatal error that was logged).
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub fps: u32,
    pub kbps: u32,
    /// Frames the phone reported as dropped before delivery (discontinuities), cumulative.
    pub gaps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Log(String),
    State(State),
    Stats(Stats),
}

type Tx = mpsc::UnboundedSender<Event>;

fn log(tx: &Tx, text: impl Into<String>) {
    let _ = tx.send(Event::Log(text.into()));
}

/// Runs until `stop` fires. Always ends with `Event::State(State::Stopped)`.
pub async fn run(cfg: Config, tx: Tx, stop: oneshot::Receiver<()>) {
    tokio::select! {
        result = run_inner(&cfg, &tx) => {
            if let Err(message) = result {
                log(&tx, format!("❌ {message}\n"));
            }
        }
        _ = stop => log(&tx, "🛑 تم إيقاف البث.\n"),
    }
    // Dropping run_inner's future above also kills ffmpeg (kill_on_drop).
    let _ = tx.send(Event::State(State::Stopped));
}

async fn run_inner(cfg: &Config, tx: &Tx) -> Result<(), String> {
    if cfg.setup_adb {
        adb_forward(cfg).await?;
        log(tx, format!("✓ adb forward على المنفذ {}\n", cfg.port));
    }
    let mut hinted = false;
    loop {
        let _ = tx.send(Event::State(State::Connecting));
        match TcpStream::connect(("127.0.0.1", cfg.port)).await {
            Ok(stream) => {
                let frames = session(cfg, stream, tx).await?;
                if frames > 0 {
                    log(tx, "الهاتف أنهى البث. بانتظار اتصال جديد...\n");
                    hinted = false;
                } else if !hinted {
                    log(tx, "بانتظار الهاتف: افتح تطبيق Enki واضغط Start.\n");
                    hinted = true;
                }
            }
            Err(e) => {
                if !hinted {
                    log(tx, format!("تعذر الاتصال بالمنفذ {} ({e}). أعيد المحاولة...\n", cfg.port));
                    hinted = true;
                }
            }
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn adb_forward(cfg: &Config) -> Result<(), String> {
    let spec = format!("tcp:{}", cfg.port);
    let out = Command::new(&cfg.adb)
        .args(["forward", &spec, &spec])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "adb غير مثبت (ثبّت android-tools / adb).".to_string()
            } else {
                format!("تعذر تشغيل adb: {e}")
            }
        })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("no devices") || stderr.contains("unauthorized") {
        Err("لا يوجد هاتف جاهز: وصّله بـ USB، فعّل USB debugging، ووافق على نافذة التصريح في الهاتف.".into())
    } else {
        Err(format!("فشل adb forward: {}", stderr.trim()))
    }
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    bytes: AtomicU64,
    gaps: AtomicU64,
}

/// One connection to the phone. Returns the number of video frames received (0 means the phone side
/// was not listening). Fatal problems (bad protocol, ffmpeg failure) are returned as `Err`.
async fn session(cfg: &Config, mut stream: TcpStream, tx: &Tx) -> Result<u64, String> {
    let _ = stream.set_nodelay(true);
    if stream.write_all(&Preamble::new(Channel::Media).encode()).await.is_err() {
        return Ok(0);
    }
    let mut reply = [0u8; PREAMBLE_LEN];
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut reply)).await {
        Ok(Ok(_)) => {}
        // `adb forward` accepts the connection even if the phone app is not listening, then closes it.
        _ => return Ok(0),
    }
    let peer = Preamble::decode(&reply).map_err(|e| format!("الهاتف يردّ ببروتوكول غير متوافق: {e}"))?;
    if peer.channel != Channel::Media {
        return Err("الهاتف ردّ بقناة غير متوقعة".into());
    }

    let (program, args) = sink_command(cfg);
    let mut child = Command::new(&program)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("{program} غير مثبت أو غير موجود في PATH.")
            } else {
                format!("فشل تشغيل {program}: {e}")
            }
        })?;
    let mut sink = child.stdin.take().ok_or("تعذر فتح مدخل ffmpeg")?;
    if let Some(stderr) = child.stderr.take() {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log(&tx, format!("⚠️ {line}\n"));
            }
        });
    }

    log(tx, format!("✓ اتصل بالهاتف، البث إلى {}\n", cfg.device));
    let _ = tx.send(Event::State(State::Streaming));

    let counters = Arc::new(Counters::default());
    let ticker = tokio::spawn(report_stats(counters.clone(), tx.clone()));
    let result = pump_frames(&mut stream, &mut sink, &counters).await;
    ticker.abort();

    let frames = counters.frames.load(Ordering::Relaxed);
    match result {
        Ok(()) => {}
        Err(PumpError::Protocol(e)) => return Err(format!("بيانات تالفة من الهاتف: {e}")),
        Err(PumpError::SinkClosed) => {
            let status = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
            return Err(format!(
                "توقف ffmpeg ({}). راجع السجل أعلاه؛ تأكد أن {} موجود وأن v4l2loopback محمّل.",
                match status {
                    Ok(Ok(s)) => s.to_string(),
                    _ => "غير معروف".into(),
                },
                cfg.device
            ));
        }
        Err(PumpError::PhoneClosed) => {}
    }
    // Closing stdin lets the decoder drain what is left and exit; only kill it if it does not.
    drop(sink);
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    Ok(frames)
}

enum PumpError {
    PhoneClosed,
    SinkClosed,
    Protocol(enki_protocol::Error),
}

async fn pump_frames(
    stream: &mut TcpStream,
    sink: &mut tokio::process::ChildStdin,
    counters: &Counters,
) -> Result<(), PumpError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut payload = Vec::new();
    loop {
        stream.read_exact(&mut header).await.map_err(|_| PumpError::PhoneClosed)?;
        let h = FrameHeader::decode(&header).map_err(PumpError::Protocol)?;
        payload.resize(h.len as usize, 0);
        stream.read_exact(&mut payload).await.map_err(|_| PumpError::PhoneClosed)?;

        // Only camera video goes to the decoder; audio arrives in a later phase.
        if h.stream_id != 0 || !matches!(h.kind, Kind::VideoConfig | Kind::VideoKey | Kind::VideoDelta) {
            continue;
        }
        if h.flags & FLAG_DISCONTINUITY != 0 {
            counters.gaps.fetch_add(1, Ordering::Relaxed);
        }
        sink.write_all(&payload).await.map_err(|_| PumpError::SinkClosed)?;
        if h.kind != Kind::VideoConfig {
            counters.frames.fetch_add(1, Ordering::Relaxed);
        }
        counters.bytes.fetch_add(h.len as u64, Ordering::Relaxed);
    }
}

async fn report_stats(counters: Arc<Counters>, tx: Tx) {
    let mut last_frames = 0;
    let mut last_bytes = 0;
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.tick().await;
    loop {
        interval.tick().await;
        let frames = counters.frames.load(Ordering::Relaxed);
        let bytes = counters.bytes.load(Ordering::Relaxed);
        let stats = Stats {
            fps: (frames - last_frames) as u32,
            kbps: ((bytes - last_bytes) * 8 / 1000) as u32,
            gaps: counters.gaps.load(Ordering::Relaxed),
        };
        last_frames = frames;
        last_bytes = bytes;
        if tx.send(Event::Stats(stats)).is_err() {
            break;
        }
    }
}

fn sink_command(cfg: &Config) -> (String, Vec<String>) {
    match &cfg.sink_override {
        Some((program, args)) => (program.clone(), args.clone()),
        None => (cfg.ffmpeg.clone(), ffmpeg_args(&cfg.device, cfg.fps)),
    }
}

/// ffmpeg arguments tuned for live use: no probing delay and no decoder-side buffering.
/// `-framerate` matters: without it ffmpeg cannot timestamp the raw H.264 and drops almost every frame.
pub fn ffmpeg_args(device: &str, fps: u32) -> Vec<String> {
    [
        "-hide_banner", "-loglevel", "warning",
        "-fflags", "nobuffer", "-flags", "low_delay",
        "-probesize", "200000", "-analyzeduration", "0",
        "-framerate", &fps.to_string(),
        "-f", "h264", "-i", "-",
        "-pix_fmt", "yuv420p", "-fps_mode", "passthrough",
        "-f", "v4l2", device,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// V4L2 devices on this system as `(path, name)`, ordered by number.
pub fn list_v4l2_devices() -> Vec<(String, String)> {
    let mut found: Vec<(u32, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = file.strip_prefix("video").and_then(|n| n.parse::<u32>().ok()) else { continue };
            let name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            found.push((n, name.trim().to_string()));
        }
    }
    found.sort();
    found.into_iter().map(|(n, name)| (format!("/dev/video{n}"), name)).collect()
}

/// The loopback device Enki created (its `card_label` is "Enki"), if present.
pub fn pick_default_device(devices: &[(String, String)]) -> Option<String> {
    devices.iter().find(|(_, name)| name.contains("Enki")).map(|(path, _)| path.clone())
}

#[cfg(test)]
mod tests;
