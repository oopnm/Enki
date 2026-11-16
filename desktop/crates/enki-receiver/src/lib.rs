//! Receives the Enki Link camera stream from the phone and feeds it to a V4L2 loopback device.
//!
//! Flow: `adb forward` → hand the phone a fresh session token over adb → connect to the phone app →
//! Enki Link handshake and authentication → H.264 frames are piped into ffmpeg, which decodes them and
//! writes to `/dev/videoN`. The session reconnects by itself when the
//! phone app is stopped and started again, and runs until [`run`]'s stop signal fires.
//!
//! Everything is reported through [`Event`]s, so any UI (GTK today) can show the state.

use enki_protocol::{
    auth_mac, to_hex, AuthStatus, Channel, FrameHeader, Kind, Preamble, FLAG_DISCONTINUITY, FRAME_HEADER_LEN, MAC_LEN,
    NONCE_LEN, PREAMBLE_LEN, TOKEN_LEN,
};
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
/// The phone app's content provider that accepts the session token, and only from adb (shell uid).
const PROVISION_URI: &str = "content://io.github.oopnm.enki.provision";

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
    /// Turn the sensor-driven rotation on or off.
    SetAutoRotate(bool),
}

/// Settings that can change while streaming. The decoder is restarted when they change the picture orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Live {
    facing: Facing,
    auto_rotate: bool,
    /// Clockwise rotation the phone asked for (from its orientation sensor), if it reported one.
    phone_rotation: Option<u16>,
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
    /// Follow the phone's orientation sensor: the phone says how the picture must be rotated to be upright
    /// and that overrides `rotate`. Can be switched live with [`Control::SetAutoRotate`].
    pub auto_rotate: bool,
    /// Secret for this session: given to the phone through adb and proven on every connection, so another
    /// app on the phone cannot read the camera. A fresh random value for every [`Config`].
    pub token: [u8; TOKEN_LEN],
    pub adb: String,
    pub ffmpeg: String,
    /// Run `adb forward` and hand over the token before connecting. Tests turn this off.
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
            auto_rotate: false,
            token: random_token(),
            adb: "adb".into(),
            ffmpeg: "ffmpeg".into(),
            setup_adb: true,
            sink_override: None,
        }
    }
}

/// 32 random bytes from the kernel. Reading `/dev/urandom` cannot fail on a working Linux system.
fn random_token() -> [u8; TOKEN_LEN] {
    use std::io::Read;
    let mut token = [0u8; TOKEN_LEN];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut token))
        .expect("cannot read random bytes from /dev/urandom");
    token
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
    live: tokio::sync::watch::Sender<Live>,
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
        live: tokio::sync::watch::channel(Live {
            facing: cfg.facing,
            auto_rotate: cfg.auto_rotate,
            phone_rotation: None,
        })
        .0,
        writer: tokio::sync::Mutex::new(None),
    });
    let command_task = {
        let shared = shared.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(request) = cmds.recv().await {
                match request {
                    Control::SetFacing(facing) => apply_facing(&shared, &tx, facing).await,
                    Control::SetAutoRotate(on) => {
                        shared.live.send_modify(|l| l.auto_rotate = on);
                        let text = if on { "🧭 التدوير التلقائي حسب مستشعر الهاتف: مفعّل" } else { "🧭 التدوير التلقائي: متوقف" };
                        log(&tx, format!("{text}\n"));
                    }
                }
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
    shared.live.send_modify(|l| l.facing = facing);
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

/// The rotation from an `image.rotation` state message, e.g. `{"type":"state","key":"image.rotation","value":180}`.
/// Plain string search: key order inside the JSON is not guaranteed and this avoids a JSON dependency.
fn parse_rotation(message: &str) -> Option<u16> {
    if !message.contains("\"image.rotation\"") {
        return None;
    }
    let rest = message[message.find("\"value\"")? + "\"value\"".len()..].trim_start().strip_prefix(':')?.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let value: u16 = digits.parse().ok()?;
    (value < 360 && value % 90 == 0).then_some(value)
}

fn handle_state(shared: &Shared, message: &str) {
    if let Some(rotation) = parse_rotation(message) {
        shared.live.send_modify(|l| l.phone_rotation = Some(rotation));
    }
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

/// Why a connection to the phone did not become an authenticated Enki Link channel.
#[derive(Debug, PartialEq, Eq)]
enum ConnectError {
    /// Nothing is serving on the port (`adb forward` accepts the connection and closes it), or the phone is
    /// slow to answer. Not an error to show: the loop just tries again.
    NotListening,
    /// The phone speaks another protocol version.
    OtherVersion(u8),
    /// The phone refused our answer to its challenge.
    Auth(AuthStatus),
    Protocol(String),
}

impl ConnectError {
    fn message(&self) -> String {
        match self {
            ConnectError::NotListening => "الهاتف لا يستمع على المنفذ.".into(),
            ConnectError::OtherVersion(v) => format!(
                "تطبيق Enki على الهاتف يستخدم بروتوكولاً مختلفاً (النسخة {v}، المطلوب {}). ثبّت على الهاتف تطبيق Enki من نفس إصدار برنامج الحاسوب.",
                enki_protocol::VERSION
            ),
            ConnectError::Auth(AuthStatus::NotProvisioned) => {
                "الهاتف لم يستلم مفتاح الجلسة. أعد تشغيل البث؛ وإن تكرر ذلك فحدّث تطبيق الهاتف.".into()
            }
            ConnectError::Auth(_) => "الهاتف رفض مفتاح الجلسة. أوقف البث على الهاتف ثم شغّله من جديد.".into(),
            ConnectError::Protocol(m) => format!("بروتوكول غير متوقع من الهاتف: {m}"),
        }
    }
}

async fn read_within(s: &mut TcpStream, buf: &mut [u8]) -> Result<(), ConnectError> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, s.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(ConnectError::NotListening),
    }
}

/// Opens a connection for `channel`: preambles both ways, then the phone's random challenge, our HMAC answer
/// and the phone's one-byte verdict (docs/protocol.md). The phone serves nothing before that succeeds.
async fn connect_authenticated(cfg: &Config, channel: Channel) -> Result<TcpStream, ConnectError> {
    let mut s = TcpStream::connect(("127.0.0.1", cfg.port)).await.map_err(|_| ConnectError::NotListening)?;
    let _ = s.set_nodelay(true);
    s.write_all(&Preamble::new(channel).encode()).await.map_err(|_| ConnectError::NotListening)?;
    let mut reply = [0u8; PREAMBLE_LEN];
    read_within(&mut s, &mut reply).await?;
    match Preamble::decode(&reply) {
        Ok(p) if p.channel == channel => {}
        Ok(_) => return Err(ConnectError::Protocol("قناة غير متوقعة".into())),
        Err(enki_protocol::Error::UnsupportedVersion(v)) => return Err(ConnectError::OtherVersion(v)),
        Err(e) => return Err(ConnectError::Protocol(e.to_string())),
    }
    let mut nonce = [0u8; NONCE_LEN];
    read_within(&mut s, &mut nonce).await?;
    let answer: [u8; MAC_LEN] = auth_mac(&cfg.token, channel, &nonce);
    s.write_all(&answer).await.map_err(|_| ConnectError::NotListening)?;
    let mut verdict = [0u8; 1];
    read_within(&mut s, &mut verdict).await?;
    match AuthStatus::from_u8(verdict[0]) {
        Some(AuthStatus::Ok) => Ok(s),
        Some(status) => Err(ConnectError::Auth(status)),
        None => Err(ConnectError::Protocol(format!("رمز توثيق غير معروف {}", verdict[0]))),
    }
}

/// Connects the control channel, tells the phone which camera to use and waits briefly for its
/// confirmation, so the camera that opens next is the right one. Returns a task that reads (and logs
/// errors from) the rest of the connection.
async fn open_control(cfg: &Config, shared: &Arc<Shared>, tx: &Tx) -> Result<tokio::task::JoinHandle<()>, ConnectError> {
    shared.live.send_modify(|l| l.phone_rotation = None); // a rotation from an earlier connection is stale
    let mut s = connect_authenticated(cfg, Channel::Control).await?;
    tokio::time::timeout(HANDSHAKE_TIMEOUT, read_control(&mut s))
        .await
        .map_err(|_| ConnectError::NotListening)?
        .map_err(|_| ConnectError::NotListening)?; // phone's hello

    let facing = shared.live.borrow().facing;
    write_control(&mut s, &set_facing_json(facing)).await.map_err(|_| ConnectError::NotListening)?;
    let _ = tokio::time::timeout(ACK_TIMEOUT, async {
        // The phone sends its picture rotation first and then confirms the camera.
        while let Ok(message) = read_control(&mut s).await {
            handle_state(shared, &message);
            if message.contains("\"camera.facing\"") {
                break;
            }
        }
    })
    .await;

    let (mut read_half, write_half) = s.into_split();
    *shared.writer.lock().await = Some(write_half);
    let tx = tx.clone();
    let shared = shared.clone();
    Ok(tokio::spawn(async move {
        while let Ok(message) = read_control(&mut read_half).await {
            handle_state(&shared, &message);
            if message.contains("\"error\"") {
                log(&tx, format!("⚠️ الهاتف: {message}\n"));
            }
        }
    }))
}

async fn run_inner(cfg: &Config, tx: &Tx, shared: &Arc<Shared>) -> Result<(), String> {
    // True right after the token was handed to the phone: a refusal then is final, not a stale token.
    let mut fresh = false;
    if cfg.setup_adb {
        adb_forward(cfg).await?;
        log(tx, format!("✓ adb forward على المنفذ {}\n", cfg.port));
        provision(cfg).await?;
        log(tx, "🔑 تم تزويد الهاتف بمفتاح جلسة جديد.\n");
        fresh = true;
    }
    let mut hinted = false;
    loop {
        let _ = tx.send(Event::State(State::Connecting));
        match open_control(cfg, shared, tx).await {
            Err(ConnectError::NotListening) => {
                if !hinted {
                    log(tx, "بانتظار الهاتف: افتح تطبيق Enki واضغط Start (وإن كان مفتوحاً فتأكد أنه من نفس إصدار برنامج الحاسوب).\n");
                    hinted = true;
                }
            }
            Err(ConnectError::Auth(status)) if cfg.setup_adb && !fresh => {
                // The phone app was restarted since we gave it the token, so it lost it: give it again.
                log(tx, format!("🔑 الهاتف لا يملك المفتاح الحالي ({status:?})، أعيد تزويده.\n"));
                provision(cfg).await?;
                fresh = true;
                continue;
            }
            Err(e) => return Err(e.message()),
            Ok(control_reader) => {
                fresh = false;
                let outcome = match connect_authenticated(cfg, Channel::Media).await {
                    Ok(stream) => session(cfg, stream, tx, shared).await,
                    Err(ConnectError::NotListening) => Ok(0),
                    Err(e) => Err(e.message()),
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

/// Gives the phone this session's token: `adb shell content call` on the app's provider, which accepts it only
/// from the adb shell. Another app on the phone cannot do that, so it never learns the token.
async fn provision(cfg: &Config) -> Result<(), String> {
    let out = Command::new(&cfg.adb)
        .args(["shell", "content", "call", "--uri", PROVISION_URI, "--method", "provision", "--arg", &to_hex(&cfg.token)])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("تعذر تشغيل adb: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() && text.contains("ok=true") {
        return Ok(());
    }
    let detail = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("لا يوجد ردّ").to_string();
    Err(format!(
        "تعذر تزويد الهاتف بمفتاح الجلسة ({detail}). تأكد أن تطبيق Enki على الهاتف من نفس إصدار برنامج الحاسوب، وأن الهاتف مصرّح له بـ adb."
    ))
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

/// Picture orientation `(clockwise degrees, mirror)` for the current settings. Defaults per camera, measured on
/// a phone held landscape with the screen facing you: back = 180° + mirror, front = 180°. With auto-rotate the
/// phone's own rotation (from its orientation sensor) replaces the degrees; `Config` overrides come first
/// when auto-rotate is off, and the mirror setting always applies.
fn orientation(cfg: &Config, live: &Live) -> (u16, bool) {
    let (default_rotate, default_mirror) = match live.facing {
        Facing::Back => (180, true),
        Facing::Front => (180, false),
    };
    let rotate = match (live.auto_rotate, live.phone_rotation) {
        (true, Some(phone)) => phone,
        _ => cfg.rotate.unwrap_or(default_rotate),
    };
    (rotate, cfg.mirror.unwrap_or(default_mirror))
}

/// The decoder process that writes to the V4L2 device.
struct Sink {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
}

impl Sink {
    async fn spawn(cfg: &Config, (rotate, mirror): (u16, bool), tx: &Tx) -> Result<Sink, String> {
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

/// One authenticated media connection to the phone. Returns the number of video frames received (0 means
/// the phone side went away at once). Fatal problems (bad protocol, ffmpeg failure) are returned as `Err`.
async fn session(cfg: &Config, mut stream: TcpStream, tx: &Tx, shared: &Shared) -> Result<u64, String> {
    let mut live_rx = shared.live.subscribe();
    let mut current = orientation(cfg, &live_rx.borrow_and_update());
    let mut sink = Sink::spawn(cfg, current, tx).await?;

    log(tx, format!("✓ اتصل بالهاتف، البث إلى {}\n", cfg.device));
    let _ = tx.send(Event::State(State::Streaming));

    let counters = Arc::new(Counters::default());
    let ticker = tokio::spawn(report_stats(counters.clone(), tx.clone()));
    let result = pump_frames(&mut stream, &mut sink, &counters, cfg, tx, &mut live_rx, &mut current).await;
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
    live_rx: &mut tokio::sync::watch::Receiver<Live>,
    current: &mut (u16, bool),
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

        // The picture orientation depends on the camera and on how the phone is held. It can only change by
        // restarting the decoder, which only happens when the resulting orientation really differs.
        if live_rx.has_changed().unwrap_or(false) {
            let wanted = orientation(cfg, &live_rx.borrow_and_update());
            if wanted != *current {
                *current = wanted;
                sink.stop().await;
                *sink = Sink::spawn(cfg, wanted, tx).await.map_err(PumpError::Fatal)?;
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
