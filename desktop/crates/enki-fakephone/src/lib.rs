//! A fake Enki phone app. It listens like the real one (preamble, authentication challenge, `hello`,
//! `capabilities`, pings, `set`) and sends H.264 frames, so the desktop receiver can be tested in CI and by
//! hand without a phone. Every behaviour the receiver must survive (old version, wrong or missing token, a
//! frozen link) can be switched on through [`Behavior`].

use enki_protocol::control::{
    Capabilities, Message, SettingEntry, Value, FEATURE_SETTINGS, MAX_CONTROL,
};
use enki_protocol::pairing::{auth_mac_tls, pairing_key, FINGERPRINT_LEN};
use enki_protocol::{
    auth_mac, to_hex, AuthStatus, Channel, FrameHeader, Kind, Preamble, FLAG_DISCONTINUITY,
    MAC_LEN, NONCE_LEN, TOKEN_LEN, VERSION,
};
use enki_settings::{Applied, Engine, HlcClock, Reject, Schema};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};

/// Raw JSON of every control message the PC sent, except pings.
pub type Received = Arc<Mutex<Vec<String>>>;

/// Where the video frames come from.
#[derive(Clone)]
pub enum MediaSource {
    /// A config, a key frame, an audio frame (which receivers must skip) and a delta frame with the
    /// discontinuity flag. Not decodable video; for tests that only look at the bytes.
    Canned,
    /// Real H.264 from a command's stdout, for example ffmpeg's `testsrc2`.
    Command(String, Vec<String>),
}

/// The phone's side of settings sync: one engine shared by every connection (like the real app's stored
/// settings), so a setting survives a reconnect. Absent = an app from before settings sync (`set` only).
#[derive(Clone)]
pub struct PhoneSettings {
    pub device_id: String,
    pub engine: Arc<Mutex<Engine>>,
    /// Keys the phone refuses with `unsupported` (for example `camera.facing` on a phone with no front camera).
    pub refuse: Arc<Mutex<Vec<String>>>,
    changes: broadcast::Sender<SettingEntry>,
}

impl PhoneSettings {
    pub fn new(device_id: &str) -> Self {
        PhoneSettings {
            device_id: device_id.into(),
            engine: Arc::new(Mutex::new(Engine::new(
                Schema::builtin(),
                device_id,
                HlcClock::system(),
            ))),
            refuse: Arc::default(),
            changes: broadcast::channel(64).0,
        }
    }

    /// The user changes a setting on the phone: it is stored and pushed to every connected PC.
    pub fn change(&self, key: &str, value: impl Into<Value>) -> Result<(), Reject> {
        let out = self.engine.lock().unwrap().set_local(key, &value.into())?;
        if let Some(entry) = out.send {
            let _ = self.changes.send(entry);
        }
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        self.engine.lock().unwrap().get(key)
    }
}

/// The phone's Wi-Fi side: a self-signed certificate, the secrets of the computers it paired with, and the key
/// of a pairing code typed on it. One value is shared by every connection, like the real app's stored state.
#[derive(Clone)]
pub struct TlsPhone {
    acceptor: tokio_rustls::TlsAcceptor,
    pub fingerprint: [u8; FINGERPRINT_LEN],
    pub device_id: String,
    /// Secrets of paired computers (they authenticate on the control and media channels).
    pub paired: Arc<Mutex<Vec<[u8; 32]>>>,
    /// The key made from the pairing code, valid for one pairing.
    pub offer: Arc<Mutex<Option<[u8; 32]>>>,
    /// The names PCs gave when pairing.
    pub pc_names: Arc<Mutex<Vec<String>>>,
    /// Wrong answers seen while an offer was open; the real phone drops the offer after five.
    pub failures: Arc<std::sync::atomic::AtomicU32>,
}

impl TlsPhone {
    pub fn new(device_id: &str) -> Self {
        let certified = rcgen::generate_simple_self_signed(vec!["enki.local".to_string()])
            .expect("a self-signed certificate");
        let cert = certified.cert.der().clone();
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()),
        );
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .expect("a valid certificate and key");
        use sha2::{Digest, Sha256};
        TlsPhone {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            fingerprint: Sha256::digest(cert.as_ref()).into(),
            device_id: device_id.into(),
            paired: Arc::default(),
            offer: Arc::default(),
            pc_names: Arc::default(),
            failures: Arc::default(),
        }
    }

    /// The user typed this code on the phone's "Pair a computer" screen.
    pub fn offer_code(&self, code: &str) {
        *self.offer.lock().unwrap() = pairing_key(code);
        self.failures.store(0, Ordering::Relaxed);
    }

    /// Makes a secret for a computer that asked to pair.
    fn pair_reply(&self, pc_name: &str) -> Message {
        let secret = random32();
        self.paired.lock().unwrap().push(secret);
        self.pc_names.lock().unwrap().push(pc_name.to_string());
        Message::Paired {
            secret: to_hex(&secret),
            fingerprint: to_hex(&self.fingerprint),
            name: "Fake phone".into(),
            device_id: self.device_id.clone(),
            port: 28201,
            hosts: vec!["127.0.0.1".into()],
        }
    }
}

fn random32() -> [u8; 32] {
    use std::io::Read;
    let mut out = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut out))
        .expect("/dev/urandom");
    out
}

/// What a connection proved at the challenge, which decides what it may do.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// The adb session token (plain loopback), or a paired secret over TLS: the whole protocol.
    Full,
    /// Over TLS with the pairing key: only `pair`.
    Offer,
}

trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

#[derive(Clone)]
pub struct Behavior {
    /// After the canned frames keep sending deltas until the receiver hangs up (otherwise close at once).
    pub hold_media: bool,
    /// Protocol version in the phone's preamble. Anything but [`VERSION`] simulates an old app: answer and hang up.
    pub version: u8,
    /// The token the phone holds, asked at every authentication. `None` = never provisioned.
    pub token: Arc<dyn Fn() -> Option<[u8; TOKEN_LEN]> + Send + Sync>,
    /// Accept any answer (for running the desktop app by hand, where the token is random and not shared).
    pub accept_any_token: bool,
    pub device: String,
    pub capabilities: Option<Capabilities>,
    /// Rotation reported after a `set` (like the real app's orientation sensor).
    pub rotation: Option<u16>,
    /// `false` = keep the connection open but never answer a ping: a link that froze.
    pub answer_pings: Arc<AtomicBool>,
    pub media: MediaSource,
    /// `Some` = the phone speaks settings sync (announced in `hello.features`).
    pub settings: Option<PhoneSettings>,
    /// The phone's Wi-Fi state. With [`Behavior::tls_listener`] the listener speaks TLS and authenticates with the
    /// paired secrets and the pairing key; without it the plain listener still answers `pair` (the USB flow).
    pub tls: Option<TlsPhone>,
    pub tls_listener: bool,
}

impl Behavior {
    pub fn new(token: [u8; TOKEN_LEN]) -> Self {
        Behavior {
            hold_media: false,
            version: VERSION,
            token: Arc::new(move || Some(token)),
            accept_any_token: false,
            device: "Fake phone".into(),
            capabilities: Some(sample_capabilities()),
            rotation: Some(90),
            answer_pings: Arc::new(AtomicBool::new(true)),
            media: MediaSource::Canned,
            settings: None,
            tls: None,
            tls_listener: false,
        }
    }
}

/// One back and one front camera with typical values.
pub fn sample_capabilities() -> Capabilities {
    use enki_protocol::control::CameraCaps;
    let camera = |id: &str, facing: &str, sensor| CameraCaps {
        id: id.into(),
        facing: facing.into(),
        sensor_orientation: sensor,
        sizes: vec![[1920, 1080], [1280, 720], [640, 480]],
        fps_ranges: vec![[15, 30], [30, 30]],
        zoom_max: 4.0,
        flash: facing == "back",
        focus_modes: vec!["continuous".into(), "auto".into()],
        ev_range: [-4, 4],
        ev_step: 0.5,
    };
    Capabilities {
        cameras: vec![camera("0", "back", 90), camera("1", "front", 270)],
        codecs: vec!["h264".into()],
    }
}

/// Accepts connections until the listener fails. Records the control messages in `received`.
pub async fn serve(listener: TcpListener, behavior: Behavior, received: Received) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let b = behavior.clone();
        let received = received.clone();
        match (&b.tls, b.tls_listener) {
            (Some(tls), true) => {
                let tls = tls.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = tls.acceptor.accept(stream).await {
                        let fp = tls.fingerprint;
                        handle(conn, b, received, Some(fp)).await;
                    }
                });
            }
            _ => {
                tokio::spawn(handle(stream, b, received, None));
            }
        }
    }
}

/// Binds an ephemeral port on loopback and serves in the background. Returns the port.
pub async fn spawn(behavior: Behavior) -> (u16, Received) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let received: Received = Arc::default();
    tokio::spawn(serve(listener, behavior, received.clone()));
    (port, received)
}

static NONCES: AtomicU64 = AtomicU64::new(1);

async fn handle<S: Stream>(
    mut s: S,
    b: Behavior,
    received: Received,
    fp: Option<[u8; FINGERPRINT_LEN]>,
) {
    let mut pre = [0u8; 8];
    if s.read_exact(&mut pre).await.is_err() {
        return;
    }
    let Ok(channel) = Preamble::decode(&pre).map(|p| p.channel) else {
        return;
    };
    let mut reply = pre;
    reply[4] = b.version;
    let _ = s.write_all(&reply).await;
    if b.version != VERSION {
        return; // like a phone that does not know our version: answer and hang up
    }
    let mut nonce = [0u8; NONCE_LEN];
    nonce[..8].copy_from_slice(&NONCES.fetch_add(1, Ordering::Relaxed).to_be_bytes());
    let _ = s.write_all(&nonce).await;
    let mut answer = [0u8; MAC_LEN];
    if s.read_exact(&mut answer).await.is_err() {
        return;
    }
    let mut level = Level::Full;
    let status = match (&fp, &b.tls) {
        (Some(fp), Some(tls)) => {
            let paired = tls.paired.lock().unwrap().clone();
            let offer = *tls.offer.lock().unwrap();
            if paired
                .iter()
                .any(|k| auth_mac_tls(k, channel, fp, &nonce) == answer)
            {
                AuthStatus::Ok
            } else if let Some(k) = offer.filter(|k| auth_mac_tls(k, channel, fp, &nonce) == answer)
            {
                let _ = k;
                level = Level::Offer;
                AuthStatus::Ok
            } else if offer.is_none() {
                AuthStatus::NotProvisioned
            } else {
                // Like the real phone: five wrong answers close the offer.
                if tls.failures.fetch_add(1, Ordering::Relaxed) + 1 >= 5 {
                    *tls.offer.lock().unwrap() = None;
                }
                AuthStatus::BadToken
            }
        }
        _ => match (b.token)() {
            None => AuthStatus::NotProvisioned,
            Some(_) if b.accept_any_token => AuthStatus::Ok,
            Some(t) if auth_mac(&t, channel, &nonce) == answer => AuthStatus::Ok,
            Some(_) => AuthStatus::BadToken,
        },
    };
    let _ = s.write_all(&[status as u8]).await;
    let _ = s.flush().await;
    if status != AuthStatus::Ok {
        return;
    }
    if level == Level::Offer {
        // Only a control connection may pair, and the offer is spent by it.
        if channel == Channel::Control {
            offer_control(s, &b, received).await;
        }
        return;
    }
    match channel {
        Channel::Control => control(s, b, received).await,
        Channel::Media => media(s, b).await,
    }
}

async fn send<W: AsyncWrite + Unpin>(s: &mut W, m: &Message) -> bool {
    s.write_all(&m.encode()).await.is_ok()
}

async fn read_json<R: AsyncRead + Unpin>(s: &mut R) -> Option<String> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.ok()?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CONTROL {
        return None;
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await.ok()?;
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// Like the real app: `hello`, then `capabilities` (and, with settings sync, the settings snapshot); a `set` is
/// answered with the current picture rotation first and then the setting's confirmation; a `ping` with a `pong`.
async fn control<S: Stream>(s: S, b: Behavior, received: Received) {
    let (mut rd, mut wr) = tokio::io::split(s);
    let hello = Message::Hello {
        version: VERSION as u32,
        app: "enki-fakephone".into(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        device: b.device.clone(),
        android: 0,
        device_id: b
            .settings
            .as_ref()
            .map(|p| p.device_id.clone())
            .unwrap_or_default(),
        features: if b.settings.is_some() {
            vec![FEATURE_SETTINGS.to_string()]
        } else {
            vec![]
        },
    };
    if !send(&mut wr, &hello).await {
        return;
    }
    if let Some(caps) = &b.capabilities {
        if !send(&mut wr, &Message::Capabilities(caps.clone())).await {
            return;
        }
    }
    if let Some(p) = &b.settings {
        let snapshot = Message::SettingsSnapshot {
            schema: 1,
            reply: false,
            settings: p.engine.lock().unwrap().snapshot(),
        };
        if !send(&mut wr, &snapshot).await {
            return;
        }
    }
    // Reading on its own task keeps the select below cancel-safe (a half-read message is never dropped).
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(text) = read_json(&mut rd).await {
            if in_tx.send(text).is_err() {
                break;
            }
        }
    });
    let mut changes = b.settings.as_ref().map(|p| p.changes.subscribe());
    loop {
        let text = tokio::select! {
            text = in_rx.recv() => match text { Some(t) => t, None => return },
            entry = next_change(&mut changes) => {
                if !send(&mut wr, &Message::Setting(entry)).await {
                    return;
                }
                continue;
            }
        };
        match Message::from_json(&text) {
            Ok(Message::Ping { seq }) => {
                if b.answer_pings.load(Ordering::Relaxed)
                    && !send(&mut wr, &Message::Pong { seq }).await
                {
                    return;
                }
            }
            Ok(Message::Pair { name, .. }) => {
                received.lock().unwrap().push(text);
                if let Some(tls) = &b.tls {
                    let reply = tls.pair_reply(&name);
                    if !send(&mut wr, &reply).await {
                        return;
                    }
                }
            }
            Ok(Message::Set { key, value }) => {
                received.lock().unwrap().push(text);
                if let Some(rotation) = b.rotation {
                    let _ = send(
                        &mut wr,
                        &Message::State {
                            key: "image.rotation".into(),
                            value: rotation.into(),
                        },
                    )
                    .await;
                }
                let _ = send(&mut wr, &Message::State { key, value }).await;
            }
            Ok(
                message @ (Message::Setting(_)
                | Message::SettingsSnapshot { .. }
                | Message::SettingAck { .. }
                | Message::SettingReject { .. }),
            ) => {
                received.lock().unwrap().push(text);
                if let Some(p) = &b.settings {
                    if !settings_message(&mut wr, p, &b, message).await {
                        return;
                    }
                }
            }
            _ => received.lock().unwrap().push(text),
        }
    }
}

/// A connection that proved only the pairing key: waits for `pair`, answers it once, spends the offer.
async fn offer_control<S: Stream>(mut s: S, b: &Behavior, received: Received) {
    let Some(tls) = &b.tls else { return };
    let text = tokio::time::timeout(Duration::from_secs(5), read_json(&mut s)).await;
    let Ok(Some(text)) = text else { return };
    received.lock().unwrap().push(text.clone());
    if let Ok(Message::Pair { name, .. }) = Message::from_json(&text) {
        *tls.offer.lock().unwrap() = None;
        let reply = tls.pair_reply(&name);
        let _ = send(&mut s, &reply).await;
    }
}

async fn next_change(changes: &mut Option<broadcast::Receiver<SettingEntry>>) -> SettingEntry {
    loop {
        match changes {
            Some(rx) => match rx.recv().await {
                Ok(entry) => return entry,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            },
            None => std::future::pending::<()>().await,
        }
    }
}

/// Like the real app, after the camera changed: the picture rotation first, then the camera confirmation.
async fn confirm_facing<W: AsyncWrite + Unpin>(
    wr: &mut W,
    p: &PhoneSettings,
    b: &Behavior,
) -> bool {
    if let Some(rotation) = b.rotation {
        if !send(
            wr,
            &Message::State {
                key: "image.rotation".into(),
                value: rotation.into(),
            },
        )
        .await
        {
            return false;
        }
    }
    let facing = p.get("camera.facing").unwrap_or_else(|| "back".into());
    send(
        wr,
        &Message::State {
            key: "camera.facing".into(),
            value: facing,
        },
    )
    .await
}

/// Handles one settings message from the PC. Returns false when the connection is gone.
async fn settings_message<W: AsyncWrite + Unpin>(
    wr: &mut W,
    p: &PhoneSettings,
    b: &Behavior,
    message: Message,
) -> bool {
    match message {
        Message::Setting(entry) => {
            let refused = p.refuse.lock().unwrap().contains(&entry.key);
            let outcome = if refused {
                Err(Reject::new(
                    enki_settings::RejectCode::Unsupported,
                    format!("{} is not supported on this phone", entry.key),
                ))
            } else {
                p.engine.lock().unwrap().apply_remote(&entry)
            };
            match outcome {
                Ok(Applied::Changed { key, .. }) => {
                    let ok = send(
                        wr,
                        &Message::SettingAck {
                            key: entry.key.clone(),
                            hlc: entry.hlc,
                            device: entry.device.clone(),
                        },
                    )
                    .await;
                    ok && (key != "camera.facing" || confirm_facing(wr, p, b).await)
                }
                Ok(Applied::Unchanged) => {
                    send(
                        wr,
                        &Message::SettingAck {
                            key: entry.key.clone(),
                            hlc: entry.hlc,
                            device: entry.device.clone(),
                        },
                    )
                    .await
                }
                Ok(Applied::Stale(newer)) => send(wr, &Message::Setting(newer)).await,
                Err(reject) => {
                    send(
                        wr,
                        &Message::SettingReject {
                            key: entry.key,
                            hlc: entry.hlc,
                            device: entry.device,
                            code: reject.code.as_str().into(),
                            message: reject.message,
                        },
                    )
                    .await
                }
            }
        }
        // The PC's answer to our snapshot: only what is newer on its side or missing here. Applied, never answered.
        Message::SettingsSnapshot {
            reply: true,
            settings,
            ..
        } => {
            let merge = p.engine.lock().unwrap().merge_snapshot(&settings);
            let facing_changed = merge.changed.iter().any(|(k, _)| k == "camera.facing");
            !facing_changed || confirm_facing(wr, p, b).await
        }
        Message::SettingAck { key, hlc, device } => {
            p.engine.lock().unwrap().acked(&key, hlc, &device);
            true
        }
        Message::SettingReject {
            key,
            hlc,
            device,
            code,
            message,
        } => {
            let reject = Reject::new(enki_settings::RejectCode::parse(&code), message);
            p.engine
                .lock()
                .unwrap()
                .rejected(&key, hlc, &device, &reject);
            true
        }
        _ => true,
    }
}

async fn write_frame<S: AsyncWrite + Unpin>(
    s: &mut S,
    stream: u8,
    kind: Kind,
    flags: u8,
    pts: u64,
    payload: &[u8],
) -> bool {
    let h = FrameHeader {
        stream_id: stream,
        kind,
        flags,
        len: payload.len() as u32,
        pts_us: pts,
    };
    s.write_all(&h.encode()).await.is_ok() && s.write_all(payload).await.is_ok()
}

async fn media<S: Stream>(mut s: S, b: Behavior) {
    match &b.media {
        MediaSource::Canned => {
            let _ = write_frame(&mut s, 0, Kind::VideoConfig, 0, 0, &[0, 0, 1, 0x67]).await;
            let _ = write_frame(&mut s, 0, Kind::VideoKey, 0, 0, &[0, 0, 1, 0x65]).await;
            let _ = write_frame(&mut s, 1, Kind::AudioFrame, 0, 5, &[9, 9, 9]).await; // must be ignored
            let _ = write_frame(
                &mut s,
                0,
                Kind::VideoDelta,
                FLAG_DISCONTINUITY,
                33_333,
                &[0x41, 0x9a],
            )
            .await;
            // Keep streaming until the receiver hangs up (restart tests need frames to keep arriving).
            if b.hold_media {
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if !write_frame(&mut s, 0, Kind::VideoDelta, 0, 66_666, &[0x41, 0x9a]).await {
                        break;
                    }
                }
            }
        }
        MediaSource::Command(program, args) => {
            let Ok(mut child) = Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            else {
                return;
            };
            let Some(mut out) = child.stdout.take() else {
                return;
            };
            let mut splitter = NalSplitter::default();
            let mut buf = vec![0u8; 64 * 1024];
            let mut pts = 0u64;
            loop {
                let n = match out.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                for frame in splitter.push(&buf[..n]) {
                    let pts_now = pts;
                    if frame.kind != Kind::VideoConfig {
                        pts += 33_333;
                    }
                    if !write_frame(&mut s, 0, frame.kind, 0, pts_now, &frame.data).await {
                        return;
                    }
                }
            }
        }
    }
}

/// One Enki Link video frame cut out of an Annex-B stream.
#[derive(Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub kind: Kind,
    pub data: Vec<u8>,
}

/// Cuts an Annex-B H.264 stream into Enki Link frames: SPS+PPS become one config frame, an IDR slice a key
/// frame, any other slice a delta frame. SEI and delimiters are dropped.
#[derive(Default)]
pub struct NalSplitter {
    pending: Vec<u8>,
    config: Vec<u8>,
}

impl NalSplitter {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<VideoFrame> {
        self.pending.extend_from_slice(bytes);
        let mut starts = Vec::new();
        let p = &self.pending;
        let mut i = 0;
        while i + 3 <= p.len() {
            if p[i] == 0 && p[i + 1] == 0 && p[i + 2] == 1 {
                starts.push(i);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut frames = Vec::new();
        // Everything before the last start code is complete; the last NAL may still be growing.
        for w in starts.windows(2) {
            let mut end = w[1];
            if end > w[0] && self.pending[end - 1] == 0 {
                end -= 1; // the 4-byte start code's leading zero belongs to the next NAL
            }
            let nal = self.pending[w[0]..end].to_vec();
            self.accept(nal, &mut frames);
        }
        if let Some(&last) = starts.last() {
            self.pending.drain(..last);
        }
        frames
    }

    fn accept(&mut self, nal: Vec<u8>, frames: &mut Vec<VideoFrame>) {
        let Some(&header) = nal.get(3) else { return };
        match header & 0x1f {
            7 | 8 => self.config.extend_from_slice(&nal),
            5 => {
                if !self.config.is_empty() {
                    frames.push(VideoFrame {
                        kind: Kind::VideoConfig,
                        data: std::mem::take(&mut self.config),
                    });
                }
                frames.push(VideoFrame {
                    kind: Kind::VideoKey,
                    data: nal,
                });
            }
            1 => frames.push(VideoFrame {
                kind: Kind::VideoDelta,
                data: nal,
            }),
            _ => {}
        }
    }
}

/// ffmpeg arguments for a 720p30 test pattern as Annex-B H.264 on stdout, with an IDR every second.
pub fn testsrc_command(ffmpeg: &str) -> MediaSource {
    let args = [
        "-loglevel",
        "error",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=1280x720:rate=30",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-tune",
        "zerolatency",
        "-bf",
        "0",
        "-g",
        "30",
        "-pix_fmt",
        "yuv420p",
        "-f",
        "h264",
        "-",
    ];
    MediaSource::Command(ffmpeg.into(), args.iter().map(|s| s.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_into_config_key_and_delta_frames() {
        let stream: Vec<u8> = [
            &[0, 0, 0, 1, 0x67, 1, 2][..], // SPS (4-byte start code)
            &[0, 0, 1, 0x68, 3][..],       // PPS
            &[0, 0, 1, 0x06, 9][..],       // SEI: dropped
            &[0, 0, 0, 1, 0x65, 4, 5][..], // IDR
            &[0, 0, 1, 0x41, 6][..],       // delta
            &[0, 0, 1, 0x41, 7][..], // delta (last one is still growing until the next start code)
        ]
        .concat();
        let mut s = NalSplitter::default();
        // Feed in odd chunks: the result must not depend on where the reads split.
        let mut frames = Vec::new();
        for chunk in stream.chunks(3) {
            frames.extend(s.push(chunk));
        }
        frames.extend(s.push(&[0, 0, 1, 0x41, 8]));
        let kinds: Vec<Kind> = frames.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Kind::VideoConfig,
                Kind::VideoKey,
                Kind::VideoDelta,
                Kind::VideoDelta
            ]
        );
        assert_eq!(frames[0].data, vec![0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3]);
        assert_eq!(frames[1].data, vec![0, 0, 1, 0x65, 4, 5]); // 3-byte start codes are what decoders need
        assert_eq!(frames[2].data, vec![0, 0, 1, 0x41, 6]);
    }
}
