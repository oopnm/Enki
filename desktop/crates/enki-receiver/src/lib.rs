//! Receives the Enki Link camera stream from the phone and feeds it to a V4L2 loopback device.
//!
//! Flow: `adb forward` → hand the phone a fresh session token over adb → connect to the phone app →
//! Enki Link handshake and authentication → H.264 frames are piped into ffmpeg, which decodes them and
//! writes to `/dev/videoN`. The session reconnects by itself when the
//! phone app is stopped and started again, and runs until [`run`]'s stop signal fires.
//!
//! Everything is reported through [`Event`]s, so any UI (GTK today) can show the state.

use enki_protocol::control::{Message, SettingEntry, Value, FEATURE_SETTINGS};
use enki_protocol::{
    auth_mac, to_hex, AuthStatus, Channel, FrameHeader, Kind, Preamble, FLAG_DISCONTINUITY,
    FRAME_HEADER_LEN, MAC_LEN, NONCE_LEN, PREAMBLE_LEN, TOKEN_LEN,
};
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Notify};

mod look;
pub use look::{Codec, Filters, Look};

use enki_settings::{Applied, Hub, Reject, RejectCode, SettingsHandle};

pub use enki_protocol::control::Capabilities;
pub use enki_settings;

pub const DEFAULT_PORT: u16 = 28200;
const RETRY_DELAY: Duration = Duration::from_millis(1500);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for the phone to confirm a setting before streaming anyway (older apps never confirm).
const ACK_TIMEOUT: Duration = Duration::from_secs(1);
/// How long to wait for a phone that announced settings sync to send its snapshot.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the PC pings the phone, and how long without any message from the phone counts as a lost link.
const PING_INTERVAL: Duration = Duration::from_secs(1);
const LINK_TIMEOUT: Duration = Duration::from_secs(3);
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
}

/// Requests a UI can send to a running session.
#[derive(Debug, Clone, PartialEq)]
pub enum Control {
    /// Switch the phone camera, also while streaming (same as setting `camera.facing`).
    SetFacing(Facing),
    /// Turn the sensor-driven rotation on or off (same as setting `video.auto_rotate`).
    SetAutoRotate(bool),
    /// Change any synced or local setting by its schema key. The change is stored, applied and sent to the phone.
    Set(String, Value),
}

/// Settings that can change while streaming. The decoder is restarted when they change the picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Live {
    facing: Facing,
    auto_rotate: bool,
    /// Fixed rotation chosen by the user (`None` = the default for the active camera).
    rotate: Option<u16>,
    /// Mirror chosen by the user (`None` = the default for the active camera).
    mirror: Option<bool>,
    /// Clockwise rotation the phone asked for (from its orientation sensor), if it reported one.
    phone_rotation: Option<u16>,
    flip: bool,
    filters: Filters,
    fps: u32,
    /// Output height: 480, 720 or 1080.
    resolution: u32,
    /// What the phone's encoder produces now (it announces it with `stream.codec`).
    codec: Codec,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            facing: Facing::Back,
            auto_rotate: false,
            rotate: None,
            mirror: None,
            phone_rotation: None,
            flip: false,
            filters: Filters::default(),
            fps: 30,
            resolution: 720,
            codec: Codec::H264,
        }
    }
}

/// What the phone reports about itself, for the window's status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhoneInfo {
    /// Battery level in percent.
    pub battery: Option<u8>,
    /// Battery temperature in tenths of a degree Celsius.
    pub temp_dc: Option<i32>,
    pub charging: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Target V4L2 device, e.g. `/dev/video10`.
    pub device: String,
    pub port: u16,
    /// Frame rate the decoder assumes (the stream carries no reliable rate in the H.264 headers).
    /// Unused since the frame rate became the synced setting `video.fps`; kept so callers still compile.
    pub fps: u32,
    /// Which phone camera to use at the start (can be changed live with [`Control::SetFacing`]). When it differs
    /// from the stored `camera.facing` it counts as the user's choice now and replaces it.
    pub facing: Facing,
    /// Rotate the picture clockwise (0, 90, 180, 270). `None` = leave the stored `video.rotate` as it is.
    pub rotate: Option<u16>,
    /// Mirror the picture horizontally. `None` = leave the stored `video.mirror` as it is.
    pub mirror: Option<bool>,
    /// Follow the phone's orientation sensor: the phone says how the picture must be rotated to be upright
    /// and that overrides `rotate`. Replaces the stored `video.auto_rotate` only when true.
    pub auto_rotate: bool,
    /// The settings this session reads and writes: shared with the window, so a change made in either place shows
    /// in the other. A fresh in-memory set for every [`Config`] unless the window gives its own.
    pub settings: SettingsHandle,
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
            settings: Hub::in_memory(enki_settings::random_device_id()).handle(),
            token: random_token(),
            adb: "adb".into(),
            ffmpeg: "ffmpeg".into(),
            // ENKI_NO_ADB=1 skips adb and the token hand-over: for talking to `enki-fakephone --accept-any-token`.
            setup_adb: std::env::var_os("ENKI_NO_ADB").is_none(),
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
    /// Round trip of the last ping, in milliseconds.
    pub rtt_ms: Option<u32>,
    /// The bit rate the phone's encoder currently aims for (it lowers it when the link cannot keep up).
    pub bitrate_kbps: Option<u32>,
}

/// Where the connection to the phone stands, for the UI's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkState {
    /// `adb forward` and handing the phone the session token.
    Preparing,
    /// The phone app is not accepting connections yet.
    WaitingForApp,
    /// Authenticated; the camera is being opened.
    Ready,
    Streaming,
    /// The link dropped (cable, app stopped, no heartbeat); trying again. Carries the reason.
    Reconnecting(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemCode {
    AdbMissing,
    /// No phone, or it has not authorised this computer for USB debugging.
    NoDevice,
    ProvisionFailed,
    /// The phone app speaks another protocol version.
    VersionMismatch,
    /// The phone refused the session token.
    AuthRejected,
    FfmpegMissing,
    /// ffmpeg could not write to the V4L2 device (module not loaded, wrong number).
    OutputDevice,
    Other,
}

/// A problem that ended the session, with a code the UI can attach a fix to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub code: ProblemCode,
    pub message: String,
}

impl Problem {
    fn new(code: ProblemCode, message: impl Into<String>) -> Self {
        Problem {
            code,
            message: message.into(),
        }
    }
}

impl From<String> for Problem {
    fn from(message: String) -> Self {
        Problem::new(ProblemCode::Other, message)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Log(String),
    State(State),
    Stats(Stats),
    Link(LinkState),
    /// What the phone's cameras can do (sent once per connection).
    Capabilities(Capabilities),
    /// A setting now has this value because the phone (or the exchange at connect time) changed it: refresh the control.
    Setting {
        key: String,
        value: Value,
    },
    /// The phone refused a change made here; the setting went back to `value`. Show `message` once.
    SettingRolledBack {
        key: String,
        value: Value,
        code: String,
        message: String,
    },
    /// The phone's battery and temperature (sent when they change).
    Phone(PhoneInfo),
    /// The session ended because of this; also logged.
    Problem(Problem),
}

type Tx = mpsc::UnboundedSender<Event>;

fn log(tx: &Tx, text: impl Into<String>) {
    let _ = tx.send(Event::Log(text.into()));
}

/// State shared between the session loop and the command task.
struct Shared {
    live: tokio::sync::watch::Sender<Live>,
    hub: SettingsHandle,
    /// The phone on this connection announced settings sync in its `hello`.
    peer_settings: std::sync::atomic::AtomicBool,
    /// Write half of the current control connection (None while not connected).
    writer: tokio::sync::Mutex<Option<OwnedWriteHalf>>,
    epoch: Instant,
    /// Milliseconds since `epoch` at which the phone last sent anything on the control channel.
    last_seen_ms: AtomicU64,
    /// Round trip of the last answered ping in milliseconds (0 = none yet).
    rtt_ms: AtomicU32,
    /// Bit rate the phone reported for its encoder, in kbit/s (0 = unknown).
    bitrate_kbps: AtomicU32,
    /// Battery and temperature last reported by the phone.
    phone: std::sync::Mutex<PhoneInfo>,
    /// Sequence number of the last ping and when it was sent.
    ping: std::sync::Mutex<(u64, Option<Instant>)>,
}

impl Shared {
    fn new(live: Live, hub: SettingsHandle) -> Self {
        Shared {
            live: tokio::sync::watch::channel(live).0,
            hub,
            peer_settings: std::sync::atomic::AtomicBool::new(false),
            writer: tokio::sync::Mutex::new(None),
            epoch: Instant::now(),
            last_seen_ms: AtomicU64::new(0),
            rtt_ms: AtomicU32::new(0),
            bitrate_kbps: AtomicU32::new(0),
            phone: std::sync::Mutex::new(PhoneInfo::default()),
            ping: std::sync::Mutex::new((0, None)),
        }
    }

    fn touch(&self) {
        self.last_seen_ms
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn since_seen(&self) -> Duration {
        let last = Duration::from_millis(self.last_seen_ms.load(Ordering::Relaxed));
        self.epoch.elapsed().saturating_sub(last)
    }

    /// Starts a new ping and returns its sequence number.
    fn next_ping(&self) -> u64 {
        let mut p = self.ping.lock().unwrap();
        p.0 += 1;
        p.1 = Some(Instant::now());
        p.0
    }

    fn pong(&self, seq: u64) {
        let p = self.ping.lock().unwrap();
        if let (true, Some(sent)) = (p.0 == seq, p.1) {
            self.rtt_ms.store(
                (sent.elapsed().as_millis() as u32).max(1),
                Ordering::Relaxed,
            );
        }
    }

    /// A new connection starts with nothing known about the phone.
    fn reset_link(&self) {
        self.rtt_ms.store(0, Ordering::Relaxed);
        self.bitrate_kbps.store(0, Ordering::Relaxed);
        self.peer_settings.store(false, Ordering::Relaxed);
        self.live.send_modify(|l| {
            l.phone_rotation = None; // a rotation from an earlier connection is stale
            l.codec = Codec::H264;
        });
        *self.phone.lock().unwrap() = PhoneInfo::default();
        self.touch();
    }
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
    seed_settings(&cfg);
    let shared = Arc::new(Shared::new(
        Live {
            facing: cfg.facing,
            auto_rotate: cfg.auto_rotate,
            ..Live::default()
        },
        cfg.settings.clone(),
    ));
    sync_live(&shared);
    let command_task = {
        let shared = shared.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(request) = cmds.recv().await {
                let (key, value) = match request {
                    Control::SetFacing(facing) => {
                        ("camera.facing".to_string(), Value::from(facing.as_str()))
                    }
                    Control::SetAutoRotate(on) => {
                        ("video.auto_rotate".to_string(), Value::from(on))
                    }
                    Control::Set(key, value) => (key, value),
                };
                set_setting(&shared, &tx, &key, value).await;
            }
        })
    };
    tokio::select! {
        result = run_inner(&cfg, &tx, &shared) => {
            if let Err(problem) = result {
                log(&tx, format!("❌ {}\n", problem.message));
                let _ = tx.send(Event::Problem(problem));
            }
        }
        _ = stop => log(&tx, "🛑 تم إيقاف البث.\n"),
    }
    // Dropping run_inner's future above also kills ffmpeg (kill_on_drop).
    command_task.abort();
    let _ = tx.send(Event::State(State::Stopped));
}

/// What the user chose when starting (a facing different from the stored one, a fixed rotation or mirror, auto
/// rotate on) becomes the stored setting; everything else keeps the stored value.
fn seed_settings(cfg: &Config) {
    let mut hub = cfg.settings.lock().unwrap();
    let mut put = |key: &str, value: Value| {
        let _ = hub.set_local(key, &value);
    };
    put("camera.facing", cfg.facing.as_str().into());
    if let Some(r) = cfg.rotate {
        put("video.rotate", r.to_string().into());
    }
    if let Some(m) = cfg.mirror {
        put("video.mirror", (if m { "on" } else { "off" }).into());
    }
    if cfg.auto_rotate {
        put("video.auto_rotate", true.into());
    }
}

/// Copies the settings that shape the picture into the live state the decoder watches.
fn sync_live(shared: &Shared) {
    let hub = shared.hub.lock().unwrap();
    let facing = if hub.get_string("camera.facing") == "front" {
        Facing::Front
    } else {
        Facing::Back
    };
    let rotate = hub.get_string("video.rotate").parse::<u16>().ok();
    let mirror = match hub.get_string("video.mirror").as_str() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    };
    let hundredths = |key: &str, default: i32| {
        hub.get(key)
            .and_then(|v| v.as_f64())
            .map(|v| (v * 100.0).round() as i32)
            .unwrap_or(default)
    };
    let filters = Filters {
        brightness: hundredths("filter.brightness", 0),
        contrast: hundredths("filter.contrast", 100),
        saturation: hundredths("filter.saturation", 100),
        sharpness: hundredths("filter.sharpness", 0),
        crop: hub
            .get("filter.crop")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(50) as u32,
    };
    let fps = hub.get_string("video.fps").parse::<u32>().unwrap_or(30);
    let resolution = match hub.get_string("video.resolution").as_str() {
        "480p" => 480,
        "1080p" => 1080,
        _ => 720,
    };
    let (auto_rotate, flip) = (
        hub.get_bool("video.auto_rotate"),
        hub.get_bool("video.flip"),
    );
    drop(hub);
    shared.live.send_modify(|l| {
        l.facing = facing;
        l.auto_rotate = auto_rotate;
        l.rotate = rotate;
        l.mirror = mirror;
        l.flip = flip;
        l.filters = filters;
        l.fps = fps;
        l.resolution = resolution;
    });
}

fn describe(key: &str, value: &Value) -> String {
    match value {
        Value::String(s) => format!("{key} = {s}"),
        other => format!("{key} = {other}"),
    }
}

/// A change made on this computer: stored, applied here at once and sent to the phone when it is connected. An
/// older phone (no settings sync) only understands `camera.facing`, sent as the old `set`. A refused value is logged.
async fn set_setting(shared: &Shared, tx: &Tx, key: &str, value: Value) {
    let outcome = shared.hub.lock().unwrap().set_local(key, &value);
    let out = match outcome {
        Ok(out) => out,
        Err(reject) => {
            log(tx, format!("⚠️ {}\n", reject.message));
            return;
        }
    };
    if !out.changed {
        return;
    }
    sync_live(shared);
    log(tx, format!("⚙️ {}\n", describe(key, &value)));
    let Some(entry) = out.send else { return };
    // Not connected yet: the new value reaches the phone in the exchange when the next connection is made.
    let mut writer = shared.writer.lock().await;
    let Some(w) = writer.as_mut() else { return };
    let message = if shared.peer_settings.load(Ordering::Relaxed) {
        Message::Setting(entry)
    } else if key == "camera.facing" {
        Message::set("camera.facing", entry.value)
    } else {
        return; // an old phone has no use for the other settings
    };
    if let Err(e) = write_message(w, &message).await {
        *writer = None;
        log(tx, format!("⚠️ تعذر إرسال الإعداد إلى الهاتف: {e}\n"));
    }
}

/// Sends the messages a handler asked for on the open control connection.
async fn send_all(shared: &Shared, messages: Vec<Message>) {
    if messages.is_empty() {
        return;
    }
    let mut writer = shared.writer.lock().await;
    if let Some(w) = writer.as_mut() {
        for m in messages {
            if write_message(w, &m).await.is_err() {
                *writer = None;
                return;
            }
        }
    }
}

fn reject_message(entry: &SettingEntry, reject: &Reject) -> Message {
    Message::SettingReject {
        key: entry.key.clone(),
        hlc: entry.hlc,
        device: entry.device.clone(),
        code: reject.code.as_str().into(),
        message: reject.message.clone(),
    }
}

fn valid_rotation(value: &enki_protocol::control::Value) -> Option<u16> {
    let v = value.as_u64()?;
    (v < 360 && v % 90 == 0).then_some(v as u16)
}

/// Applies one control message from the phone and returns what must be sent back (acknowledgements and the like).
/// Any message proves the link is alive.
#[must_use]
fn handle_message(shared: &Shared, tx: &Tx, message: Message) -> Vec<Message> {
    shared.touch();
    let mut replies = Vec::new();
    match message {
        Message::State { key, value } => match key.as_str() {
            "image.rotation" => {
                if let Some(rotation) = valid_rotation(&value) {
                    shared
                        .live
                        .send_modify(|l| l.phone_rotation = Some(rotation));
                }
            }
            "stream.codec" => {
                if let Some(codec) = value.as_str().and_then(Codec::from_wire) {
                    shared.live.send_modify(|l| l.codec = codec);
                }
            }
            "phone.battery" | "phone.temperature" | "phone.charging" => {
                let info = {
                    let mut info = shared.phone.lock().unwrap();
                    match key.as_str() {
                        "phone.battery" => info.battery = value.as_u64().map(|v| v.min(100) as u8),
                        "phone.temperature" => info.temp_dc = value.as_i64().map(|v| v as i32),
                        _ => info.charging = value.as_bool(),
                    }
                    *info
                };
                let _ = tx.send(Event::Phone(info));
            }
            "stream.fps_info" => {
                if let Some((chosen, ranges)) = value.as_str().and_then(|v| v.split_once('|')) {
                    let wanted = shared.live.borrow().fps;
                    let chosen: u32 = chosen.parse().unwrap_or(0);
                    let note = if chosen >= wanted {
                        format!(
                            "Phone camera runs at up to {chosen} fps (camera offers: {ranges}).\n"
                        )
                    } else {
                        format!(
                            "⚠️ {wanted} fps was asked for, but this camera only offers up to {chosen} fps \
                             in normal mode (camera offers: {ranges}), so the picture stays at {chosen} fps.\n"
                        )
                    };
                    let _ = tx.send(Event::Log(note));
                }
            }
            "stream.bitrate" => {
                if let Some(kbps) = value.as_u64() {
                    shared
                        .bitrate_kbps
                        .store(kbps.min(u32::MAX as u64) as u32, Ordering::Relaxed);
                }
            }
            _ => {}
        },
        Message::Pong { seq } => shared.pong(seq),
        Message::Hello {
            device,
            app_version,
            device_id,
            features,
            ..
        } => {
            let device_label = if device.is_empty() { "?" } else { &device };
            log(
                tx,
                format!("📱 الهاتف: {device_label} (Enki {app_version})\n"),
            );
            if features.iter().any(|f| f == FEATURE_SETTINGS) {
                shared.peer_settings.store(true, Ordering::Relaxed);
                // Each phone keeps its own synced settings; the one that connected now is the one in use.
                let changed = shared.hub.lock().unwrap().switch_phone(&device_id, &device);
                if !changed.is_empty() {
                    sync_live(shared);
                }
                for (key, value) in changed {
                    let _ = tx.send(Event::Setting { key, value });
                }
            }
        }
        Message::Capabilities(caps) => {
            let cameras: Vec<String> = caps
                .cameras
                .iter()
                .map(|c| match c.sizes.first() {
                    Some([w, h]) => format!("{} {w}x{h}", c.facing),
                    None => c.facing.clone(),
                })
                .collect();
            log(tx, format!("📷 كاميرات الهاتف: {}\n", cameras.join("، ")));
            if let Ok(json) = serde_json::to_string(&Message::Capabilities(caps.clone())) {
                shared.hub.lock().unwrap().cache_capabilities(json);
            }
            let _ = tx.send(Event::Capabilities(caps));
        }
        Message::Error { code, message } => log(tx, format!("⚠️ الهاتف: {code}: {message}\n")),
        Message::SettingsSnapshot {
            reply, settings, ..
        } => {
            let merge = shared.hub.lock().unwrap().merge_snapshot(&settings);
            for (key, reject) in &merge.ignored {
                log(
                    tx,
                    format!("ℹ️ تجاهل إعداد من الهاتف ({key}): {}\n", reject.message),
                );
            }
            if !merge.changed.is_empty() {
                sync_live(shared);
            }
            for (key, value) in merge.changed {
                let _ = tx.send(Event::Setting { key, value });
            }
            // The phone's own snapshot is answered once, with only what it lacks or has older. Its reply is not.
            if !reply {
                replies.push(Message::SettingsSnapshot {
                    schema: 1,
                    reply: true,
                    settings: merge.send_back,
                });
            }
        }
        Message::Setting(entry) => {
            let outcome = shared.hub.lock().unwrap().apply_remote(&entry);
            match outcome {
                Ok(Applied::Changed { key, value }) => {
                    sync_live(shared);
                    log(tx, format!("📲 من الهاتف: {}\n", describe(&key, &value)));
                    let _ = tx.send(Event::Setting { key, value });
                    replies.push(Message::SettingAck {
                        key: entry.key,
                        hlc: entry.hlc,
                        device: entry.device,
                    });
                }
                Ok(Applied::Unchanged) => replies.push(Message::SettingAck {
                    key: entry.key,
                    hlc: entry.hlc,
                    device: entry.device,
                }),
                Ok(Applied::Stale(newer)) => replies.push(Message::Setting(newer)),
                Err(reject) => replies.push(reject_message(&entry, &reject)),
            }
        }
        Message::SettingAck { key, hlc, device } => {
            shared.hub.lock().unwrap().acked(&key, hlc, &device);
        }
        Message::SettingReject {
            key,
            hlc,
            device,
            code,
            message,
        } => {
            let reject = Reject::new(RejectCode::parse(&code), message);
            let rolled = shared
                .hub
                .lock()
                .unwrap()
                .rejected(&key, hlc, &device, &reject);
            if let Some(r) = rolled {
                sync_live(shared);
                log(tx, format!("⚠️ الهاتف رفض {key}: {}\n", reject.message));
                let _ = tx.send(Event::SettingRolledBack {
                    key: r.key,
                    value: r.value,
                    code,
                    message: reject.message,
                });
            }
        }
        _ => {}
    }
    replies
}

async fn write_message<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    message: &Message,
) -> std::io::Result<()> {
    w.write_all(&message.encode()).await?;
    w.flush().await
}

/// One control message. Text that is not a known message (a newer phone) comes back as [`Message::Unknown`].
async fn read_message<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Message> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > enki_protocol::control::MAX_CONTROL {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "control message too large",
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Message::from_json(&String::from_utf8_lossy(&body)).unwrap_or(Message::Unknown))
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
    fn problem(&self) -> Problem {
        let (code, message) = match self {
            ConnectError::NotListening => (ProblemCode::Other, "الهاتف لا يستمع على المنفذ.".to_string()),
            ConnectError::OtherVersion(v) => (
                ProblemCode::VersionMismatch,
                format!(
                    "تطبيق Enki على الهاتف يستخدم بروتوكولاً مختلفاً (النسخة {v}، المطلوب {}). ثبّت على الهاتف تطبيق Enki من نفس إصدار برنامج الحاسوب.",
                    enki_protocol::VERSION
                ),
            ),
            ConnectError::Auth(AuthStatus::NotProvisioned) => (
                ProblemCode::AuthRejected,
                "الهاتف لم يستلم مفتاح الجلسة. أعد تشغيل البث؛ وإن تكرر ذلك فحدّث تطبيق الهاتف.".to_string(),
            ),
            ConnectError::Auth(_) => (
                ProblemCode::AuthRejected,
                "الهاتف رفض مفتاح الجلسة. أوقف البث على الهاتف ثم شغّله من جديد.".to_string(),
            ),
            ConnectError::Protocol(m) => (ProblemCode::Other, format!("بروتوكول غير متوقع من الهاتف: {m}")),
        };
        Problem::new(code, message)
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
    let mut s = TcpStream::connect(("127.0.0.1", cfg.port))
        .await
        .map_err(|_| ConnectError::NotListening)?;
    let _ = s.set_nodelay(true);
    s.write_all(&Preamble::new(channel).encode())
        .await
        .map_err(|_| ConnectError::NotListening)?;
    let mut reply = [0u8; PREAMBLE_LEN];
    read_within(&mut s, &mut reply).await?;
    match Preamble::decode(&reply) {
        Ok(p) if p.channel == channel => {}
        Ok(_) => return Err(ConnectError::Protocol("قناة غير متوقعة".into())),
        Err(enki_protocol::Error::UnsupportedVersion(v)) => {
            return Err(ConnectError::OtherVersion(v))
        }
        Err(e) => return Err(ConnectError::Protocol(e.to_string())),
    }
    let mut nonce = [0u8; NONCE_LEN];
    read_within(&mut s, &mut nonce).await?;
    let answer: [u8; MAC_LEN] = auth_mac(&cfg.token, channel, &nonce);
    s.write_all(&answer)
        .await
        .map_err(|_| ConnectError::NotListening)?;
    let mut verdict = [0u8; 1];
    read_within(&mut s, &mut verdict).await?;
    match AuthStatus::from_u8(verdict[0]) {
        Some(AuthStatus::Ok) => Ok(s),
        Some(status) => Err(ConnectError::Auth(status)),
        None => Err(ConnectError::Protocol(format!(
            "رمز توثيق غير معروف {}",
            verdict[0]
        ))),
    }
}

/// Connects the control channel, tells the phone which camera to use and waits briefly for its
/// confirmation, so the camera that opens next is the right one. Returns a task that reads the rest of the
/// connection (rotation, bit rate, pongs, errors) until the phone closes it.
async fn open_control(
    cfg: &Config,
    shared: &Arc<Shared>,
    tx: &Tx,
) -> Result<tokio::task::JoinHandle<()>, ConnectError> {
    shared.reset_link();
    let mut s = connect_authenticated(cfg, Channel::Control).await?;
    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_message(&mut s))
        .await
        .map_err(|_| ConnectError::NotListening)?
        .map_err(|_| ConnectError::NotListening)?;
    // `hello` carries no reply. It also tells us which phone this is, which switches the settings profile.
    let _ = handle_message(shared, tx, hello);

    if shared.peer_settings.load(Ordering::Relaxed) {
        // Settings sync: the phone sends its capabilities and its settings snapshot; we merge, answer with only
        // what the phone lacks or has older, and, when that includes the camera, wait for the phone to switch.
        let mut sent_facing = false;
        let _ = tokio::time::timeout(SNAPSHOT_TIMEOUT, async {
            while let Ok(message) = read_message(&mut s).await {
                let is_snapshot =
                    matches!(&message, Message::SettingsSnapshot { reply: false, .. });
                for reply in handle_message(shared, tx, message) {
                    if let Message::SettingsSnapshot { settings, .. } = &reply {
                        sent_facing |= settings.iter().any(|e| e.key == "camera.facing");
                    }
                    if write_message(&mut s, &reply).await.is_err() {
                        return;
                    }
                }
                if is_snapshot {
                    break;
                }
            }
        })
        .await;
        if sent_facing {
            let _ = tokio::time::timeout(ACK_TIMEOUT, async {
                while let Ok(message) = read_message(&mut s).await {
                    let confirmed =
                        matches!(&message, Message::State { key, .. } if key == "camera.facing");
                    for reply in handle_message(shared, tx, message) {
                        let _ = write_message(&mut s, &reply).await;
                    }
                    if confirmed {
                        break;
                    }
                }
            })
            .await;
        }
    } else {
        // An older phone app: tell it which camera to use and wait briefly for its confirmation.
        let facing = shared.live.borrow().facing;
        write_message(&mut s, &Message::set("camera.facing", facing.as_str()))
            .await
            .map_err(|_| ConnectError::NotListening)?;
        let _ = tokio::time::timeout(ACK_TIMEOUT, async {
            // The phone sends its capabilities and picture rotation first and then confirms the camera.
            while let Ok(message) = read_message(&mut s).await {
                let confirmed =
                    matches!(&message, Message::State { key, .. } if key == "camera.facing");
                let _ = handle_message(shared, tx, message);
                if confirmed {
                    break;
                }
            }
        })
        .await;
    }

    let (mut read_half, write_half) = s.into_split();
    *shared.writer.lock().await = Some(write_half);
    let tx = tx.clone();
    let shared = shared.clone();
    Ok(tokio::spawn(async move {
        while let Ok(message) = read_message(&mut read_half).await {
            let replies = handle_message(&shared, &tx, message);
            send_all(&shared, replies).await;
        }
    }))
}

/// Pings the phone every second. Fires `dead` when the phone has said nothing for [`LINK_TIMEOUT`] (a pulled
/// cable or a frozen app: TCP alone may not notice for minutes) or when a ping cannot be written.
async fn heartbeat(shared: Arc<Shared>, dead: Arc<Notify>) {
    shared.touch();
    let mut tick = tokio::time::interval(PING_INTERVAL);
    tick.tick().await;
    loop {
        tick.tick().await;
        if shared.since_seen() > LINK_TIMEOUT {
            dead.notify_one();
            return;
        }
        let seq = shared.next_ping();
        let mut writer = shared.writer.lock().await;
        let failed = match writer.as_mut() {
            Some(w) => write_message(w, &Message::Ping { seq }).await.is_err(),
            None => false,
        };
        if failed {
            *writer = None;
            dead.notify_one();
            return;
        }
    }
}

/// Sends a [`LinkState`] only when it differs from the last one sent.
struct LinkTracker(Option<LinkState>);

impl LinkTracker {
    fn set(&mut self, tx: &Tx, state: LinkState) {
        if self.0.as_ref() != Some(&state) {
            self.0 = Some(state.clone());
            let _ = tx.send(Event::Link(state));
        }
    }
}

async fn run_inner(cfg: &Config, tx: &Tx, shared: &Arc<Shared>) -> Result<(), Problem> {
    let mut link = LinkTracker(None);
    // True right after the token was handed to the phone: a refusal then is final, not a stale token.
    let mut fresh = false;
    if cfg.setup_adb {
        link.set(tx, LinkState::Preparing);
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
                link.set(tx, LinkState::WaitingForApp);
                if !hinted {
                    log(tx, "بانتظار الهاتف: افتح تطبيق Enki واضغط Start (وإن كان مفتوحاً فتأكد أنه من نفس إصدار برنامج الحاسوب).\n");
                    hinted = true;
                }
            }
            Err(ConnectError::Auth(status)) if cfg.setup_adb && !fresh => {
                // The phone app was restarted since we gave it the token, so it lost it: give it again.
                log(
                    tx,
                    format!("🔑 الهاتف لا يملك المفتاح الحالي ({status:?})، أعيد تزويده.\n"),
                );
                link.set(tx, LinkState::Preparing);
                provision(cfg).await?;
                fresh = true;
                continue;
            }
            Err(e) => return Err(e.problem()),
            Ok(control_reader) => {
                let mut control_reader = AbortOnDrop(control_reader);
                fresh = false;
                link.set(tx, LinkState::Ready);
                let dead = Arc::new(Notify::new());
                let _beat = AbortOnDrop(tokio::spawn(heartbeat(shared.clone(), dead.clone())));
                let outcome = tokio::select! {
                    r = async {
                        match connect_authenticated(cfg, Channel::Media).await {
                            Ok(stream) => session(cfg, stream, tx, shared).await,
                            Err(ConnectError::NotListening) => Ok(0),
                            Err(e) => Err(e.problem()),
                        }
                    } => r,
                    _ = dead.notified() => {
                        log(tx, "⚠️ انقطع الاتصال بالهاتف (لا ردّ على النبض). أعيد الاتصال...\n");
                        link.set(tx, LinkState::Reconnecting("no heartbeat".into()));
                        Ok(0)
                    }
                    _ = &mut control_reader.0 => {
                        log(tx, "⚠️ الهاتف أغلق قناة التحكم. أعيد الاتصال...\n");
                        link.set(tx, LinkState::Reconnecting("control channel closed".into()));
                        Ok(0)
                    }
                };
                *shared.writer.lock().await = None;
                if outcome? > 0 {
                    log(tx, "الهاتف أنهى البث. بانتظار اتصال جديد...\n");
                    link.set(tx, LinkState::Reconnecting("phone ended the stream".into()));
                    hinted = false;
                } else if link.0 == Some(LinkState::Ready) || link.0 == Some(LinkState::Streaming) {
                    link.0 = None; // the next round reports where it stands
                }
            }
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

/// Gives the phone this session's token: `adb shell content call` on the app's provider, which accepts it only
/// from the adb shell. Another app on the phone cannot do that, so it never learns the token.
async fn provision(cfg: &Config) -> Result<(), Problem> {
    let out = Command::new(&cfg.adb)
        .args([
            "shell",
            "content",
            "call",
            "--uri",
            PROVISION_URI,
            "--method",
            "provision",
            "--arg",
            &to_hex(&cfg.token),
        ])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Problem::new(
                    ProblemCode::AdbMissing,
                    "adb غير مثبت (ثبّت android-tools / adb).",
                )
            } else {
                Problem::new(ProblemCode::Other, format!("تعذر تشغيل adb: {e}"))
            }
        })?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() && text.contains("ok=true") {
        return Ok(());
    }
    let detail = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("لا يوجد ردّ")
        .to_string();
    Err(Problem::new(
        ProblemCode::ProvisionFailed,
        format!("تعذر تزويد الهاتف بمفتاح الجلسة ({detail}). تأكد أن تطبيق Enki على الهاتف من نفس إصدار برنامج الحاسوب، وأن الهاتف مصرّح له بـ adb."),
    ))
}

async fn adb_forward(cfg: &Config) -> Result<(), Problem> {
    let spec = format!("tcp:{}", cfg.port);
    let out = Command::new(&cfg.adb)
        .args(["forward", &spec, &spec])
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Problem::new(
                    ProblemCode::AdbMissing,
                    "adb غير مثبت (ثبّت android-tools / adb).",
                )
            } else {
                Problem::new(ProblemCode::Other, format!("تعذر تشغيل adb: {e}"))
            }
        })?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("no devices") || stderr.contains("unauthorized") {
        Err(Problem::new(
            ProblemCode::NoDevice,
            "لا يوجد هاتف جاهز: وصّله بـ USB، فعّل USB debugging، ووافق على نافذة التصريح في الهاتف.",
        ))
    } else {
        Err(Problem::new(
            ProblemCode::Other,
            format!("فشل adb forward: {}", stderr.trim()),
        ))
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
fn orientation(live: &Live) -> (u16, bool) {
    let (default_rotate, default_mirror) = match live.facing {
        Facing::Back => (180, true),
        Facing::Front => (180, false),
    };
    let rotate = match (live.auto_rotate, live.phone_rotation) {
        (true, Some(phone)) => phone,
        _ => live.rotate.unwrap_or(default_rotate),
    };
    (rotate, live.mirror.unwrap_or(default_mirror))
}

/// The decoder process that writes to the V4L2 device.
struct Sink {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
}

impl Sink {
    async fn spawn(cfg: &Config, look: &Look, tx: &Tx) -> Result<Sink, Problem> {
        let (program, args) = sink_command(cfg, look);
        let mut child = Command::new(&program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Problem::new(
                        ProblemCode::FfmpegMissing,
                        format!("{program} غير مثبت أو غير موجود في PATH."),
                    )
                } else {
                    Problem::new(ProblemCode::Other, format!("فشل تشغيل {program}: {e}"))
                }
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| Problem::from("تعذر فتح مدخل ffmpeg".to_string()))?;
        if let Some(stderr) = child.stderr.take() {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(&tx, format!("⚠️ {line}\n"));
                }
            });
        }
        Ok(Sink {
            child,
            stdin: Some(stdin),
        })
    }

    /// Writes one piece of the phone's stream to the decoder.
    async fn feed(&mut self, payload: &[u8]) -> Result<(), ()> {
        let stdin = self.stdin.as_mut().ok_or(())?;
        stdin.write_all(payload).await.map_err(|_| ())
    }

    async fn stop(&mut self) {
        self.stdin = None;
        if tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// One authenticated media connection to the phone. Returns the number of video frames received (0 means
/// the phone side went away at once). Fatal problems (bad protocol, ffmpeg failure) are returned as `Err`.
async fn session(
    cfg: &Config,
    mut stream: TcpStream,
    tx: &Tx,
    shared: &Arc<Shared>,
) -> Result<u64, Problem> {
    let mut live_rx = shared.live.subscribe();
    let mut current = look(&live_rx.borrow_and_update());
    let mut sink = Sink::spawn(cfg, &current, tx).await?;

    log(tx, format!("✓ اتصل بالهاتف، البث إلى {}\n", cfg.device));
    let _ = tx.send(Event::State(State::Streaming));
    let _ = tx.send(Event::Link(LinkState::Streaming));

    let counters = Arc::new(Counters::default());
    // Stops with this function even when the future is dropped (stop pressed, heartbeat lost): a detached ticker
    // would keep reporting "connected, 0 fps" long after the phone is gone.
    let _ticker = AbortOnDrop(tokio::spawn(report_stats(
        counters.clone(),
        shared.clone(),
        tx.clone(),
    )));
    let result = pump_frames(
        &mut stream,
        &mut sink,
        &counters,
        cfg,
        tx,
        &mut live_rx,
        &mut current,
    )
    .await;

    let frames = counters.frames.load(Ordering::Relaxed);
    match result {
        Ok(()) | Err(PumpError::PhoneClosed) => {}
        Err(PumpError::Protocol(e)) => {
            sink.stop().await;
            return Err(Problem::new(
                ProblemCode::Other,
                format!("بيانات تالفة من الهاتف: {e}"),
            ));
        }
        Err(PumpError::Fatal(message)) => return Err(message),
        Err(PumpError::SinkClosed) => {
            let status = tokio::time::timeout(Duration::from_secs(1), sink.child.wait()).await;
            return Err(Problem::new(
                ProblemCode::OutputDevice,
                format!(
                    "توقف ffmpeg ({}). راجع السجل أعلاه؛ تأكد أن {} موجود وأن v4l2loopback محمّل.",
                    match status {
                        Ok(Ok(s)) => s.to_string(),
                        _ => "غير معروف".into(),
                    },
                    cfg.device
                ),
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
    Fatal(Problem),
}

#[allow(clippy::too_many_arguments)]
async fn pump_frames(
    stream: &mut TcpStream,
    sink: &mut Sink,
    counters: &Counters,
    cfg: &Config,
    tx: &Tx,
    live_rx: &mut tokio::sync::watch::Receiver<Live>,
    current: &mut Look,
) -> Result<(), PumpError> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    let mut payload = Vec::new();
    // After a restart the new decoder must start at a config frame (the phone sends one before every key frame).
    let mut wait_for_config = false;
    loop {
        stream
            .read_exact(&mut header)
            .await
            .map_err(|_| PumpError::PhoneClosed)?;
        let h = FrameHeader::decode(&header).map_err(PumpError::Protocol)?;
        payload.resize(h.len as usize, 0);
        stream
            .read_exact(&mut payload)
            .await
            .map_err(|_| PumpError::PhoneClosed)?;

        // The picture orientation depends on the camera and on how the phone is held. It can only change by
        // restarting the decoder, which only happens when the resulting orientation really differs.
        if live_rx.has_changed().unwrap_or(false) {
            let mut wanted = look(&live_rx.borrow_and_update());
            wanted.codec = current.codec; // the codec comes from the stream itself, see below
            if wanted != *current {
                *current = wanted;
                sink.stop().await;
                *sink = Sink::spawn(cfg, &wanted, tx)
                    .await
                    .map_err(PumpError::Fatal)?;
                wait_for_config = true;
            }
        }

        // Only camera video goes to the decoder; audio arrives in a later phase.
        if h.stream_id != 0
            || !matches!(
                h.kind,
                Kind::VideoConfig | Kind::VideoKey | Kind::VideoDelta
            )
        {
            continue;
        }
        // The parameter sets say which codec this is. Trusting them (and not a separate message that may arrive
        // late) means the decoder always starts with the right demuxer.
        if h.kind == Kind::VideoConfig {
            if let Some(codec) = Codec::detect(&payload) {
                if codec != current.codec {
                    current.codec = codec;
                    sink.stop().await;
                    *sink = Sink::spawn(cfg, current, tx)
                        .await
                        .map_err(PumpError::Fatal)?;
                    wait_for_config = false; // this very frame is the start of the new decoder's input
                }
            }
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
        sink.feed(&payload)
            .await
            .map_err(|_| PumpError::SinkClosed)?;
        if h.kind != Kind::VideoConfig {
            counters.frames.fetch_add(1, Ordering::Relaxed);
        }
        counters.bytes.fetch_add(h.len as u64, Ordering::Relaxed);
    }
}

/// A spawned task that is aborted when this guard is dropped, including when the owning future is cancelled.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn report_stats(counters: Arc<Counters>, shared: Arc<Shared>, tx: Tx) {
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
            rtt_ms: Some(shared.rtt_ms.load(Ordering::Relaxed)).filter(|v| *v > 0),
            bitrate_kbps: Some(shared.bitrate_kbps.load(Ordering::Relaxed)).filter(|v| *v > 0),
        };
        last_frames = frames;
        last_bytes = bytes;
        if tx.send(Event::Stats(stats)).is_err() {
            break;
        }
    }
}

fn sink_command(cfg: &Config, look: &Look) -> (String, Vec<String>) {
    match &cfg.sink_override {
        Some((program, args)) => (program.clone(), args.clone()),
        None => (cfg.ffmpeg.clone(), look::ffmpeg_args_for(&cfg.device, look)),
    }
}

/// The settings that shape the picture, as the decoder needs them.
fn look(live: &Live) -> Look {
    let (rotate, mirror) = orientation(live);
    Look {
        rotate,
        mirror,
        flip: live.flip,
        filters: live.filters,
        codec: live.codec,
        fps: live.fps,
        out_height: live.resolution,
    }
}

/// Plain ffmpeg arguments for a rotation and mirror, everything else at its default (720p, 30 fps).
pub fn ffmpeg_args(device: &str, fps: u32, rotate: u16, mirror: bool) -> Vec<String> {
    look::ffmpeg_args_for(
        device,
        &Look {
            rotate,
            mirror,
            fps,
            ..Look::default()
        },
    )
}

#[cfg(test)]
fn video_filter(rotate: u16, mirror: bool) -> String {
    look::filter_chain(&Look {
        rotate,
        mirror,
        ..Look::default()
    })
}

/// V4L2 devices on this system as `(path, name)`, ordered by number.
pub fn list_v4l2_devices() -> Vec<(String, String)> {
    let mut found: Vec<(u32, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(n) = file
                .strip_prefix("video")
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let name = std::fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            found.push((n, name.trim().to_string()));
        }
    }
    found.sort();
    found
        .into_iter()
        .map(|(n, name)| (format!("/dev/video{n}"), name))
        .collect()
}

/// The loopback device Enki created (its `card_label` is "Enki"), if present.
pub fn pick_default_device(devices: &[(String, String)]) -> Option<String> {
    devices
        .iter()
        .find(|(_, name)| name.contains("Enki"))
        .map(|(path, _)| path.clone())
}

#[cfg(test)]
mod tests;
