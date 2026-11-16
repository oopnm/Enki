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
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

pub const DEFAULT_PORT: u16 = 28200;
const RETRY_DELAY: Duration = Duration::from_millis(1500);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the phone to confirm a setting before streaming anyway (older apps never confirm).
const ACK_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONTROL: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Facing {
    Back,
    Front,
}

impl Facing {
    pub fn as_str(self) -> &'static str {
        match self {
            Facing::Back => "back",
            Facing::Front => "front",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Facing::Back => "الخلفية",
            Facing::Front => "الأمامية",
        }
    }
}

/// Requests a UI can send to a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    /// Switch the phone camera, also while streaming.
    SetFacing(Facing),
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Target V4L2 device, e.g. `/dev/video10`.
    pub device: String,
    pub port: u16,
    /// Frame rate the decoder assumes (the stream carries no reliable rate in the H.264 headers).
    pub fps: u32,
    /// Which phone camera to use at the start (can be changed live with [`Control::SetFacing`]).
    pub facing: Facing,
    /// Rotate the picture clockwise (0, 90, 180, 270). `None` = the default for the active camera.
    pub rotate: Option<u16>,
    /// Mirror the picture horizontally. `None` = the default for the active camera.
    pub mirror: Option<bool>,
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
            facing: Facing::Back,
            rotate: None,
            mirror: None,
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

/// State shared between the session loop and the command task.
struct Shared {
    facing: tokio::sync::watch::Sender<Facing>,
    /// Write half of the current control connection (None while not connected).
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
}

/// Runs until `stop` fires. Always ends with `Event::State(State::Stopped)`.
pub async fn run(cfg: Config, tx: Tx, stop: oneshot::Receiver<()>) {
    let (_keep_open, cmds) = mpsc::unbounded_channel();
    run_with_commands(cfg, tx, stop, cmds).await;
}

/// Like [`run`], and also applies [`Control`] messages (for example a live camera switch) while running.
pub async fn run_with_commands(
    cfg: Config,
    tx: Tx,
    stop: oneshot::Receiver<()>,
    mut cmds: mpsc::UnboundedReceiver<Control>,
) {
    let shared = Arc::new(Shared {
        facing: tokio::sync::watch::channel(cfg.facing).0,
        writer: tokio::sync::Mutex::new(None),
    });
    let command_task = {
        let shared = shared.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(Control::SetFacing(facing)) = cmds.recv().await {
                apply_facing(&shared, &tx, facing).await;
            }
        })
    };
    tokio::select! {
        result = run_inner(&cfg, &tx, &shared) => {
            if let Err(message) = result {
                log(&tx, format!("❌ {message}\n"));
            }
        }
        _ = stop => log(&tx, "🛑 تم إيقاف البث.\n"),
    }
    // Dropping run_inner's future above also kills ffmpeg (kill_on_drop).
    command_task.abort();
    let _ = tx.send(Event::State(State::Stopped));
}

async fn apply_facing(shared: &Shared, tx: &Tx, facing: Facing) {
    shared.facing.send_replace(facing);
    let mut writer = shared.writer.lock().await;
    if let Some(w) = writer.as_mut() {
        match write_control(w, &set_facing_json(facing)).await {
            Ok(()) => log(tx, format!("🔄 تبديل الكاميرا إلى {}\n", facing.label())),
            Err(e) => {
                *writer = None;
                log(tx, format!("⚠️ تعذر إرسال تبديل الكاميرا إلى الهاتف: {e}\n"));
            }
        }
    }
    // Not connected yet: the new value is sent as soon as the next connection is made.
}

fn set_facing_json(facing: Facing) -> String {
    format!(r#"{{"type":"set","key":"camera.facing","value":"{}"}}"#, facing.as_str())
}

/// Control messages are a big-endian u32 length followed by UTF-8 JSON (docs/protocol.md).
async fn write_control<W: AsyncWriteExt + Unpin>(w: &mut W, json: &str) -> std::io::Result<()> {
    let body = json.as_bytes();
    let mut msg = Vec::with_capacity(4 + body.len());
    msg.extend_from_slice(&(body.len() as u32).to_be_bytes());
    msg.extend_from_slice(body);
    w.write_all(&msg).await?;
    w.flush().await
}

async fn read_control<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<String> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CONTROL {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "control message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Connects the control channel, tells the phone which camera to use and waits briefly for its
/// confirmation, so the camera that opens next is the right one. Returns a task that reads (and logs
/// errors from) the rest of the connection, or None if the phone app is not listening.
async fn open_control(cfg: &Config, shared: &Arc<Shared>, tx: &Tx) -> Option<tokio::task::JoinHandle<()>> {
    let mut s = TcpStream::connect(("127.0.0.1", cfg.port)).await.ok()?;
    s.write_all(&Preamble::new(Channel::Control).encode()).await.ok()?;
    let mut reply = [0u8; PREAMBLE_LEN];
    // `adb forward` accepts the connection even if the phone app is not listening, then closes it.
    tokio::time::timeout(HANDSHAKE_TIMEOUT, s.read_exact(&mut reply)).await.ok()?.ok()?;
    Preamble::decode(&reply).ok().filter(|p| p.channel == Channel::Control)?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, read_control(&mut s)).await.ok()?.ok()?; // phone's hello

    let facing = *shared.facing.borrow();
    write_control(&mut s, &set_facing_json(facing)).await.ok()?;
    let _ = tokio::time::timeout(ACK_TIMEOUT, async {
        while let Ok(message) = read_control(&mut s).await {
            if message.contains("\"state\"") {
                break;
            }
        }
    })
    .await;

    let (mut read_half, write_half) = s.into_split();
    *shared.writer.lock().await = Some(write_half);
    let tx = tx.clone();
    Some(tokio::spawn(async move {
        while let Ok(message) = read_control(&mut read_half).await {
            if message.contains("\"error\"") {
                log(&tx, format!("⚠️ الهاتف: {message}\n"));
            }
        }
    }))
}

async fn run_inner(cfg: &Config, tx: &Tx, shared: &Arc<Shared>) -> Result<(), String> {
    if cfg.setup_adb {
        adb_forward(cfg).await?;
        log(tx, format!("✓ adb forward على المنفذ {}\n", cfg.port));
    }
    let mut hinted = false;
    loop {
        let _ = tx.send(Event::State(State::Connecting));
        match open_control(cfg, shared, tx).await {
            None => {
                if !hinted {
                    log(tx, "بانتظار الهاتف: افتح تطبيق Enki واضغط Start.\n");
                    hinted = true;
                }
            }
            Some(control_reader) => {
                let outcome = match TcpStream::connect(("127.0.0.1", cfg.port)).await {
                    Ok(stream) => session(cfg, stream, tx, shared).await,
                    Err(_) => Ok(0),
                };
                control_reader.abort();
                *shared.writer.lock().await = None;
                if outcome? > 0 {
                    log(tx, "الهاتف أنهى البث. بانتظار اتصال جديد...\n");
                    hinted = false;
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

/// Default picture orientation per camera, measured on a phone held landscape with the screen facing you:
/// the back camera needs 180° + mirror, the front camera 180°. `Config` can override both.
fn orientation(cfg: &Config, facing: Facing) -> (u16, bool) {
    let (rotate, mirror) = match facing {
        Facing::Back => (180, true),
        Facing::Front => (180, false),
    };
    (cfg.rotate.unwrap_or(rotate), cfg.mirror.unwrap_or(mirror))
}

/// The decoder process that writes to the V4L2 device.
struct Sink {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
}

impl Sink {
    async fn spawn(cfg: &Config, facing: Facing, tx: &Tx) -> Result<Sink, String> {
        let (rotate, mirror) = orientation(cfg, facing);
        let (program, args) = sink_command(cfg, rotate, mirror);
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
        let stdin = child.stdin.take().ok_or("تعذر فتح مدخل ffmpeg")?;
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(&tx, format!("⚠️ {line}\n"));
                }
            });
        }
        Ok(Sink { child, stdin: Some(stdin) })
    }

    /// Closing stdin lets the decoder drain what is left and exit; it is killed only if it does not.
    async fn stop(&mut self) {
        self.stdin = None;
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

/// One connection to the phone. Returns the number of video frames received (0 means the phone side
/// was not listening). Fatal problems (bad protocol, ffmpeg failure) are returned as `Err`.
async fn session(cfg: &Config, mut stream: TcpStream, tx: &Tx, shared: &Shared) -> Result<u64, String> {
    let _ = stream.set_nodelay(true);
    if stream.write_all(&Preamble::new(Channel::Media).encode()).await.is_err() {
        return Ok(0);
    }
    let mut reply = [0u8; PREAMBLE_LEN];
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut reply)).await {
        Ok(Ok(_)) => {}
        _ => return Ok(0),
    }
    let peer = Preamble::decode(&reply).map_err(|e| format!("الهاتف يردّ ببروتوكول غير متوافق: {e}"))?;
    if peer.channel != Channel::Media {
        return Err("الهاتف ردّ بقناة غير متوقعة".into());
    }

    let mut facing_rx = shared.facing.subscribe();
    let mut facing = *facing_rx.borrow_and_update();
    let mut sink = Sink::spawn(cfg, facing, tx).await?;

    log(tx, format!("✓ اتصل بالهاتف، البث إلى {}\n", cfg.device));
    let _ = tx.send(Event::State(State::Streaming));

    let counters = Arc::new(Counters::default());
    let ticker = tokio::spawn(report_stats(counters.clone(), tx.clone()));
    let result = pump_frames(&mut stream, &mut sink, &counters, cfg, tx, &mut facing_rx, &mut facing).await;
    ticker.abort();

    let frames = counters.frames.load(Ordering::Relaxed);
    match result {
        Ok(()) | Err(PumpError::PhoneClosed) => {}
        Err(PumpError::Protocol(e)) => {
            sink.stop().await;
            return Err(format!("بيانات تالفة من الهاتف: {e}"));
        }
        Err(PumpError::Fatal(message)) => return Err(message),
        Err(PumpError::SinkClosed) => {
            let status = tokio::time::timeout(Duration::from_secs(1), sink.child.wait()).await;
            return Err(format!(
                "توقف ffmpeg ({}). راجع السجل أعلاه؛ تأكد أن {} موجود وأن v4l2loopback محمّل.",
                match status {
                    Ok(Ok(s)) => s.to_string(),
                    _ => "غير معروف".into(),
                },
                cfg.device
            ));
        }
    }
    sink.stop().await;
    Ok(frames)
}

enum PumpError {
    PhoneClosed,
    SinkClosed,
    Protocol(enki_protocol::Error),
    Fatal(String),
}

#[allow(clippy::too_many_arguments)]
async fn pump_frames(
    stream: &mut TcpStream,
    sink: &mut Sink,
    counters: &Counters,
    cfg: &Config,
    tx: &Tx,
    facing_rx: &mut tokio::sync::watch::Receiver<Facing>,
    facing: &mut Facing,
) -> Result<(), PumpError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut payload = Vec::new();
    // After a restart the new decoder must start at a config frame (the phone sends one before every key frame).
    let mut wait_for_config = false;
    loop {
        stream.read_exact(&mut header).await.map_err(|_| PumpError::PhoneClosed)?;
        let h = FrameHeader::decode(&header).map_err(PumpError::Protocol)?;
        payload.resize(h.len as usize, 0);
        stream.read_exact(&mut payload).await.map_err(|_| PumpError::PhoneClosed)?;

        // The picture orientation differs per camera and can only change by restarting the decoder.
        if facing_rx.has_changed().unwrap_or(false) {
            let new = *facing_rx.borrow_and_update();
            let changed = orientation(cfg, new) != orientation(cfg, *facing);
            *facing = new;
            if changed {
                sink.stop().await;
                *sink = Sink::spawn(cfg, new, tx).await.map_err(PumpError::Fatal)?;
                wait_for_config = true;
            }
        }

        // Only camera video goes to the decoder; audio arrives in a later phase.
        if h.stream_id != 0 || !matches!(h.kind, Kind::VideoConfig | Kind::VideoKey | Kind::VideoDelta) {
            continue;
        }
        if wait_for_config {
            if h.kind != Kind::VideoConfig {
                continue;
            }
            wait_for_config = false;
        }
        if h.flags & FLAG_DISCONTINUITY != 0 {
            counters.gaps.fetch_add(1, Ordering::Relaxed);
        }
        let stdin = sink.stdin.as_mut().ok_or(PumpError::SinkClosed)?;
        stdin.write_all(&payload).await.map_err(|_| PumpError::SinkClosed)?;
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

fn sink_command(cfg: &Config, rotate: u16, mirror: bool) -> (String, Vec<String>) {
    match &cfg.sink_override {
        Some((program, args)) => (program.clone(), args.clone()),
        None => (cfg.ffmpeg.clone(), ffmpeg_args(&cfg.device, cfg.fps, rotate, mirror)),
    }
}

/// ffmpeg arguments tuned for live use: no probing delay and no decoder-side buffering.
/// `-framerate` matters: without it ffmpeg cannot timestamp the raw H.264 and drops almost every frame.
/// The filter chain pins the output size, so it stays constant when the phone switches camera or the picture is rotated.
pub fn ffmpeg_args(device: &str, fps: u32, rotate: u16, mirror: bool) -> Vec<String> {
    [
        "-hide_banner", "-loglevel", "warning",
        "-fflags", "nobuffer", "-flags", "low_delay",
        "-probesize", "200000", "-analyzeduration", "0",
        "-framerate", &fps.to_string(),
        "-f", "h264", "-i", "-",
        "-vf", &video_filter(rotate, mirror),
        "-pix_fmt", "yuv420p", "-fps_mode", "passthrough",
        "-f", "v4l2", device,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Rotation, mirror, then a fit into a fixed 1280x720 frame with black bars. The size never changes, whatever the
/// rotation or camera, because programs that already opened the V4L2 device expect the size they negotiated;
/// a different frame size shows up as a garbled picture.
fn video_filter(rotate: u16, mirror: bool) -> String {
    let mut chain: Vec<&str> = Vec::new();
    match rotate % 360 {
        90 => chain.push("transpose=1"),
        180 => chain.extend(["hflip", "vflip"]),
        270 => chain.push("transpose=2"),
        _ => {}
    }
    if mirror {
        chain.push("hflip");
    }
    chain.push("scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2");
    chain.push("pad=1280:720:(ow-iw)/2:(oh-ih)/2");
    chain.push("setsar=1");
    chain.join(",")
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
