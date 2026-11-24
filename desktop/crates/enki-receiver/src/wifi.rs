//! Enki Link over Wi-Fi: the paired-phone book, a TLS connection pinned to the phone's certificate, finding the
//! phone on the network (mDNS), and the two ways of pairing (a code typed on the phone, or the USB cable).
//! See docs/protocol.md, "Wi-Fi".
//!
//! The protocol inside the TLS connection is the same Enki Link as over USB. Only two things differ: the PC
//! answers the phone's challenge with [`enki_protocol::pairing::auth_mac_tls`], keyed with the secret it got at
//! pairing, and the phone's certificate fingerprint must be the pinned one.

use crate::{write_message, ConnectError, Problem, HANDSHAKE_TIMEOUT};
use enki_protocol::control::Message;
use enki_protocol::pairing::{auth_mac_tls, pairing_key, FINGERPRINT_LEN};
use enki_protocol::{
    auth_mac, from_hex, AuthStatus, Channel, Preamble, MAC_LEN, NONCE_LEN, PREAMBLE_LEN, TOKEN_LEN,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The port the phone's Wi-Fi listener uses (the USB one, 28200, is on the phone's loopback only).
pub const WIFI_PORT: u16 = 28201;
/// The mDNS service type the phone announces; its `id` text record is the phone's device id.
pub const SERVICE_TYPE: &str = "_enki._tcp.local.";
/// How long a pairing code stays valid on the phone, and so how long [`pair_with_code`] keeps trying.
pub const PAIR_WINDOW: Duration = Duration::from_secs(180);

/// A stream the protocol runs on: a plain TCP connection (USB) or TLS over one (Wi-Fi).
pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Duplex for T {}
pub type Conn = Box<dyn Duplex>;

// ---------------------------------------------------------------------------------------------------------
// The book of paired phones

/// A phone this computer has paired with. Stored in `phones.toml` next to the settings (readable only by the
/// user, since it holds the secret).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedPhone {
    /// The phone app's device id (the same id the settings sync uses).
    pub id: String,
    pub name: String,
    /// SHA-256 of the phone's certificate, hex. The connection is refused if the phone shows another.
    pub fingerprint: String,
    /// The secret that proves this computer to the phone, hex.
    pub secret: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Addresses the phone had when it was paired.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The address that worked last time.
    #[serde(default)]
    pub last_host: Option<String>,
}

fn default_port() -> u16 {
    WIFI_PORT
}

impl PairedPhone {
    pub fn fingerprint_bytes(&self) -> Option<[u8; FINGERPRINT_LEN]> {
        from_hex(&self.fingerprint)?.try_into().ok()
    }

    pub fn secret_bytes(&self) -> Option<[u8; TOKEN_LEN]> {
        from_hex(&self.secret)?.try_into().ok()
    }

    /// Addresses worth trying without mDNS: the one that worked last, then the ones from pairing.
    pub fn known_addresses(&self) -> Vec<SocketAddr> {
        let mut out: Vec<SocketAddr> = Vec::new();
        for host in self.last_host.iter().chain(self.hosts.iter()) {
            if let Ok(ip) = host.parse::<IpAddr>() {
                let addr = SocketAddr::new(ip, self.port);
                if !out.contains(&addr) {
                    out.push(addr);
                }
            }
        }
        out
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BookFile {
    #[serde(default)]
    phones: Vec<PairedPhone>,
}

/// The paired phones, stored in one TOML file. Every method reads and writes the file, so two windows never
/// disagree.
#[derive(Debug, Clone)]
pub struct PhoneBook {
    path: PathBuf,
}

impl PhoneBook {
    pub fn new(dir: impl AsRef<Path>) -> PhoneBook {
        PhoneBook {
            path: dir.as_ref().join("phones.toml"),
        }
    }

    pub fn default_book() -> PhoneBook {
        PhoneBook::new(enki_settings::Store::default_dir())
    }

    pub fn load(&self) -> Vec<PairedPhone> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| toml::from_str::<BookFile>(&t).ok())
            .map(|f| f.phones)
            .unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<PairedPhone> {
        self.load().into_iter().find(|p| p.id == id)
    }

    fn save(&self, phones: Vec<PairedPhone>) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(&BookFile { phones })
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension("toml.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Adds the phone, or replaces the entry with the same id (pairing again gives a new secret).
    pub fn upsert(&self, phone: PairedPhone) -> std::io::Result<()> {
        let mut phones = self.load();
        phones.retain(|p| p.id != phone.id);
        phones.push(phone);
        self.save(phones)
    }

    pub fn remove(&self, id: &str) -> std::io::Result<()> {
        let mut phones = self.load();
        phones.retain(|p| p.id != id);
        self.save(phones)
    }

    /// Remembers the address that worked.
    pub fn set_last_host(&self, id: &str, host: &str) -> std::io::Result<()> {
        let mut phones = self.load();
        let Some(p) = phones.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        if p.last_host.as_deref() == Some(host) {
            return Ok(());
        }
        p.last_host = Some(host.to_string());
        self.save(phones)
    }
}

// ---------------------------------------------------------------------------------------------------------
// TLS with a pinned certificate

/// Accepts the phone's self-signed certificate only if its SHA-256 is the pinned one (or, while pairing, any
/// certificate: the pairing answer then proves it is the right phone, see [`pair_with_code`]). The handshake
/// signature is still checked, so the peer must hold the certificate's private key.
#[derive(Debug)]
struct PinVerifier {
    pin: Option<[u8; FINGERPRINT_LEN]>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.pin {
            Some(pin) if fingerprint(end_entity.as_ref()) != pin => Err(
                rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure),
            ),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// SHA-256 of a DER certificate.
pub fn fingerprint(cert_der: &[u8]) -> [u8; FINGERPRINT_LEN] {
    Sha256::digest(cert_der).into()
}

fn client_config(pin: Option<[u8; FINGERPRINT_LEN]>) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("ring supports TLS 1.2 and 1.3")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier { pin, provider }))
        .with_no_client_auth();
    Arc::new(config)
}

/// Opens TCP and TLS to the phone. With a pin the phone's certificate must match it; without one any
/// certificate is accepted and its fingerprint is returned for the caller to bind into its answer.
pub(crate) async fn tls_connect(
    addr: SocketAddr,
    pin: Option<[u8; FINGERPRINT_LEN]>,
) -> Result<(Conn, [u8; FINGERPRINT_LEN]), ConnectError> {
    let tcp = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| ConnectError::NotListening)?
        .map_err(|_| ConnectError::NotListening)?;
    let _ = tcp.set_nodelay(true);
    let connector = tokio_rustls::TlsConnector::from(client_config(pin));
    let name = ServerName::try_from("enki.local").expect("a valid DNS name");
    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(name, tcp)).await {
        Err(_) => return Err(ConnectError::NotListening),
        Ok(Err(e)) => return Err(tls_error(e)),
        Ok(Ok(tls)) => tls,
    };
    let fp = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|c| fingerprint(c.as_ref()))
        .ok_or_else(|| ConnectError::Protocol("الهاتف لم يعرض شهادة".into()))?;
    Ok((Box::new(tls), fp))
}

/// A failed handshake with a pin set most likely means another certificate (a reset phone app, or someone in the
/// middle); anything else is "the phone is not serving TLS there".
fn tls_error(e: std::io::Error) -> ConnectError {
    let pinned_mismatch = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|inner| {
            matches!(
                inner,
                rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)
            )
        });
    if pinned_mismatch {
        ConnectError::CertificateChanged
    } else {
        ConnectError::NotListening
    }
}

// ---------------------------------------------------------------------------------------------------------
// The handshake shared by USB and Wi-Fi

/// What the PC answers the phone's challenge with.
pub(crate) enum AuthKey {
    /// USB: the session token handed over through adb.
    Session([u8; TOKEN_LEN]),
    /// Wi-Fi: a paired secret or a pairing key, bound to the certificate fingerprint that was seen.
    Tls {
        key: [u8; 32],
        fingerprint: [u8; FINGERPRINT_LEN],
    },
}

async fn read_within<S: AsyncRead + Unpin>(s: &mut S, buf: &mut [u8]) -> Result<(), ConnectError> {
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, s.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(ConnectError::NotListening),
    }
}

/// Preambles both ways, then the phone's random challenge, our answer and the phone's one-byte verdict
/// (docs/protocol.md). The phone serves nothing before that succeeds.
pub(crate) async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    channel: Channel,
    auth: &AuthKey,
) -> Result<(), ConnectError> {
    s.write_all(&Preamble::new(channel).encode())
        .await
        .map_err(|_| ConnectError::NotListening)?;
    s.flush().await.map_err(|_| ConnectError::NotListening)?;
    let mut reply = [0u8; PREAMBLE_LEN];
    read_within(s, &mut reply).await?;
    match Preamble::decode(&reply) {
        Ok(p) if p.channel == channel => {}
        Ok(_) => return Err(ConnectError::Protocol("قناة غير متوقعة".into())),
        Err(enki_protocol::Error::UnsupportedVersion(v)) => {
            return Err(ConnectError::OtherVersion(v))
        }
        Err(e) => return Err(ConnectError::Protocol(e.to_string())),
    }
    let mut nonce = [0u8; NONCE_LEN];
    read_within(s, &mut nonce).await?;
    let answer: [u8; MAC_LEN] = match auth {
        AuthKey::Session(token) => auth_mac(token, channel, &nonce),
        AuthKey::Tls { key, fingerprint } => auth_mac_tls(key, channel, fingerprint, &nonce),
    };
    s.write_all(&answer)
        .await
        .map_err(|_| ConnectError::NotListening)?;
    s.flush().await.map_err(|_| ConnectError::NotListening)?;
    let mut verdict = [0u8; 1];
    read_within(s, &mut verdict).await?;
    match AuthStatus::from_u8(verdict[0]) {
        Some(AuthStatus::Ok) => Ok(()),
        Some(status) => Err(ConnectError::Auth(status)),
        None => Err(ConnectError::Protocol(format!(
            "رمز توثيق غير معروف {}",
            verdict[0]
        ))),
    }
}

// ---------------------------------------------------------------------------------------------------------
// Finding the phone

/// A phone announcing itself on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The phone app's device id.
    pub id: String,
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

/// Listens for `_enki._tcp` announcements for `window` and returns what it heard. Returns nothing, instead of
/// failing, when the network has no multicast: the caller falls back to the addresses it remembers.
pub async fn discover(window: Duration) -> Vec<Found> {
    tokio::task::spawn_blocking(move || discover_blocking(window))
        .await
        .unwrap_or_default()
}

fn discover_blocking(window: Duration) -> Vec<Found> {
    use mdns_sd::{ServiceDaemon, ServiceEvent};
    let Ok(daemon) = ServiceDaemon::new() else {
        return Vec::new();
    };
    let Ok(events) = daemon.browse(SERVICE_TYPE) else {
        let _ = daemon.shutdown();
        return Vec::new();
    };
    let deadline = Instant::now() + window;
    let mut found: Vec<Found> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let id = info.get_property_val_str("id").unwrap_or("").to_string();
                let port = info.get_port();
                let mut addrs: Vec<SocketAddr> = info
                    .get_addresses_v4()
                    .into_iter()
                    .map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
                    .collect();
                addrs.sort();
                let name = info
                    .get_fullname()
                    .split('.')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if addrs.is_empty() {
                    continue;
                }
                found.retain(|f| !(f.id == id && f.name == name));
                found.push(Found { id, name, addrs });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    found
}

/// Where to try the phone, best first: what mDNS heard for its id, then the remembered addresses.
pub async fn candidates(phone: &PairedPhone, mdns_window: Duration) -> Vec<SocketAddr> {
    let mut out: Vec<SocketAddr> = Vec::new();
    if !mdns_window.is_zero() {
        for f in discover(mdns_window).await {
            if f.id == phone.id {
                for a in f.addrs {
                    if !out.contains(&a) {
                        out.push(a);
                    }
                }
            }
        }
    }
    for a in phone.known_addresses() {
        if !out.contains(&a) {
            out.push(a);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Pairing

/// Why pairing did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    /// The code is not 10 symbols from the pairing alphabet.
    BadCode,
    /// No phone took the code within [`PAIR_WINDOW`] (not found on the network, or nobody typed the code).
    NotFound,
    /// A phone answered but refused the code three times.
    WrongCode,
    /// USB pairing: the phone app is not running.
    PhoneAppNotRunning,
    /// USB pairing: adb is missing, the phone is not authorised, or the app is another version.
    Adb(Problem),
    Protocol(String),
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::BadCode => write!(f, "الرمز غير صحيح الشكل (10 خانات)"),
            PairError::NotFound => write!(f, "لم يستلم أي هاتف الرمز في الوقت المحدد. تأكد أن الهاتف والحاسوب على نفس الشبكة وأن الرمز كُتب في الهاتف."),
            PairError::WrongCode => write!(f, "الهاتف رفض الرمز. تأكد من كتابته كما يظهر هنا."),
            PairError::PhoneAppNotRunning => write!(f, "تطبيق Enki لا يعمل على الهاتف. افتحه واضغط Start ثم أعد المحاولة."),
            PairError::Adb(p) => write!(f, "{}", p.message),
            PairError::Protocol(m) => write!(f, "ردّ غير متوقع من الهاتف: {m}"),
        }
    }
}

impl std::error::Error for PairError {}

/// What the phone said it is, after the answer was checked.
fn phone_from_paired(
    message: Message,
    seen: Option<[u8; FINGERPRINT_LEN]>,
    reached: Option<SocketAddr>,
) -> Result<PairedPhone, PairError> {
    let Message::Paired {
        secret,
        fingerprint,
        name,
        device_id,
        port,
        hosts,
    } = message
    else {
        return Err(PairError::Protocol("لا رد paired".into()));
    };
    let fp = from_hex(&fingerprint)
        .and_then(|v| <[u8; FINGERPRINT_LEN]>::try_from(v).ok())
        .ok_or_else(|| PairError::Protocol("بصمة الشهادة غير صالحة".into()))?;
    if from_hex(&secret).is_none_or(|s| s.len() != TOKEN_LEN) {
        return Err(PairError::Protocol("السر غير صالح".into()));
    }
    if device_id.is_empty() {
        return Err(PairError::Protocol("الهاتف لم يرسل معرّفه".into()));
    }
    // Over Wi-Fi the fingerprint we saw must be the one the phone reports; over USB we saw none, and adb is the
    // trust anchor.
    if seen.is_some_and(|seen| seen != fp) {
        return Err(PairError::Protocol("بصمة الشهادة لا تطابق ما رأيناه".into()));
    }
    let mut hosts = hosts;
    let last_host = reached
        .map(|a| a.ip().to_string())
        .or_else(|| hosts.first().cloned());
    if let Some(h) = &last_host {
        if !hosts.contains(h) {
            hosts.insert(0, h.clone());
        }
    }
    Ok(PairedPhone {
        id: device_id,
        name: if name.is_empty() {
            "Android".into()
        } else {
            name
        },
        fingerprint,
        secret,
        port: if port == 0 { WIFI_PORT } else { port },
        hosts,
        last_host,
    })
}

async fn request_pairing<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    pc_name: &str,
    pc_id: &str,
) -> Result<Message, PairError> {
    write_message(
        s,
        &Message::Pair {
            name: pc_name.into(),
            device: pc_id.into(),
        },
    )
    .await
    .map_err(|e| PairError::Protocol(e.to_string()))?;
    // The phone may send `hello` and other messages first; wait for `paired` or `error`.
    let wait = async {
        loop {
            match crate::read_message(s).await {
                Ok(m @ Message::Paired { .. }) => return Ok(m),
                Ok(Message::Error { code, message }) => {
                    return Err(PairError::Protocol(format!("{code}: {message}")))
                }
                Ok(_) => {}
                Err(e) => return Err(PairError::Protocol(e.to_string())),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(8), wait)
        .await
        .map_err(|_| PairError::Protocol("لا رد من الهاتف".into()))?
}

/// One try at pairing with the phone at `addr` using the key from the code.
async fn pair_once(
    addr: SocketAddr,
    key: [u8; 32],
    pc_name: &str,
    pc_id: &str,
) -> Result<PairedPhone, AttemptError> {
    let (mut conn, fp) = tls_connect(addr, None)
        .await
        .map_err(AttemptError::Connect)?;
    handshake(
        &mut conn,
        Channel::Control,
        &AuthKey::Tls {
            key,
            fingerprint: fp,
        },
    )
    .await
    .map_err(AttemptError::Connect)?;
    let message = request_pairing(&mut conn, pc_name, pc_id)
        .await
        .map_err(AttemptError::Pair)?;
    phone_from_paired(message, Some(fp), Some(addr)).map_err(AttemptError::Pair)
}

enum AttemptError {
    Connect(ConnectError),
    Pair(PairError),
}

/// Pairs with the phone on which the user types `code`. The code is shown here and typed on the phone (Wi-Fi
/// screen, "Pair a computer"); the phone then accepts one pairing from whoever proves it knows the code, bound to
/// the certificate the PC sees, so nobody in the middle can take the phone's place.
///
/// Looks for phones with mDNS (and at `manual`, if given) every two seconds for up to `window` (normally [`PAIR_WINDOW`]). A phone that
/// has no code yet is skipped quietly; a phone that refuses the code three times ends the attempt.
pub async fn pair_with_code(
    code: &str,
    pc_name: &str,
    pc_id: &str,
    manual: Option<SocketAddr>,
    window: Duration,
) -> Result<PairedPhone, PairError> {
    let key = pairing_key(code).ok_or(PairError::BadCode)?;
    let deadline = Instant::now() + window;
    let mut wrong: std::collections::HashMap<SocketAddr, u32> = Default::default();
    while Instant::now() < deadline {
        let mut addrs: Vec<SocketAddr> = manual.into_iter().collect();
        if manual.is_none() {
            for f in discover(Duration::from_millis(1500)).await {
                addrs.extend(f.addrs);
            }
        }
        for addr in addrs {
            match pair_once(addr, key, pc_name, pc_id).await {
                Ok(phone) => return Ok(phone),
                Err(AttemptError::Connect(ConnectError::Auth(AuthStatus::BadToken))) => {
                    let n = wrong.entry(addr).or_default();
                    *n += 1;
                    if *n >= 3 {
                        return Err(PairError::WrongCode);
                    }
                }
                // No code on that phone yet, or not an Enki phone, or not reachable: keep looking.
                Err(AttemptError::Connect(_)) => {}
                Err(AttemptError::Pair(e)) => return Err(e),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(PairError::NotFound)
}

/// Pairs over the USB cable: `adb` is the trust anchor, so no code is needed. The phone app must be running
/// (Start pressed). Same flow as a session: forward the port, give the phone a session token, ask it to pair.
pub async fn pair_over_adb(
    adb: &str,
    port: u16,
    pc_name: &str,
    pc_id: &str,
) -> Result<PairedPhone, PairError> {
    let token = crate::random_token();
    crate::adb_forward(adb, port)
        .await
        .map_err(PairError::Adb)?;
    crate::provision(adb, &token)
        .await
        .map_err(PairError::Adb)?;
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .await
        .map_err(|_| PairError::PhoneAppNotRunning)?;
    let mut conn: Conn = Box::new(tcp);
    handshake(&mut conn, Channel::Control, &AuthKey::Session(token))
        .await
        .map_err(|e| match e {
            ConnectError::NotListening => PairError::PhoneAppNotRunning,
            other => PairError::Adb(other.problem()),
        })?;
    let message = request_pairing(&mut conn, pc_name, pc_id).await?;
    phone_from_paired(message, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use enki_protocol::to_hex;

    fn phone() -> PairedPhone {
        PairedPhone {
            id: "0123456789abcdef0123456789abcdef".into(),
            name: "Pixel".into(),
            fingerprint: to_hex(&[7u8; 32]),
            secret: to_hex(&[9u8; 32]),
            port: WIFI_PORT,
            hosts: vec!["192.168.1.20".into(), "10.0.0.5".into()],
            last_host: Some("10.0.0.5".into()),
        }
    }

    #[test]
    fn book_stores_replaces_and_forgets_phones() {
        let dir = tempfile::tempdir().unwrap();
        let book = PhoneBook::new(dir.path());
        assert!(book.load().is_empty());
        book.upsert(phone()).unwrap();
        let mut again = phone();
        again.secret = to_hex(&[1u8; 32]);
        book.upsert(again.clone()).unwrap();
        assert_eq!(book.load(), vec![again.clone()], "same id replaces");
        book.set_last_host(&again.id, "192.168.1.20").unwrap();
        assert_eq!(
            book.get(&again.id).unwrap().last_host.as_deref(),
            Some("192.168.1.20")
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.path().join("phones.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the secrets file is private");
        book.remove(&again.id).unwrap();
        assert!(book.load().is_empty());
    }

    #[test]
    fn known_addresses_put_the_last_working_one_first() {
        let addrs = phone().known_addresses();
        assert_eq!(
            addrs,
            vec![
                "10.0.0.5:28201".parse().unwrap(),
                "192.168.1.20:28201".parse().unwrap()
            ]
        );
        assert_eq!(phone().fingerprint_bytes(), Some([7u8; 32]));
        assert_eq!(phone().secret_bytes(), Some([9u8; 32]));
    }

    #[test]
    fn a_paired_reply_is_checked() {
        let good = Message::Paired {
            secret: to_hex(&[9u8; 32]),
            fingerprint: to_hex(&[7u8; 32]),
            name: "Pixel".into(),
            device_id: "abc".into(),
            port: 0,
            hosts: vec![],
        };
        let p = phone_from_paired(
            good.clone(),
            Some([7u8; 32]),
            Some("10.1.1.1:28201".parse().unwrap()),
        )
        .unwrap();
        assert_eq!(
            (p.port, p.last_host.as_deref()),
            (WIFI_PORT, Some("10.1.1.1"))
        );
        assert!(p.hosts.contains(&"10.1.1.1".to_string()));
        // The phone reports a different certificate than the one we saw: someone is in the middle.
        assert!(phone_from_paired(good.clone(), Some([8u8; 32]), None).is_err());
        // Over USB nothing was seen, so the phone's word stands.
        assert!(phone_from_paired(good, None, None).is_ok());
        let short = Message::Paired {
            secret: "00".into(),
            fingerprint: to_hex(&[7u8; 32]),
            name: String::new(),
            device_id: "abc".into(),
            port: 1,
            hosts: vec![],
        };
        assert!(phone_from_paired(short, None, None).is_err());
    }

    /// The certificate structure the phone app writes (see protocol/vectors/phone_cert.txt) is one rustls accepts.
    #[test]
    fn the_phone_certificate_structure_is_accepted_by_rustls() {
        let line = include_str!("../../../../protocol/vectors/phone_cert.txt")
            .lines()
            .find(|l| !l.starts_with('#') && !l.trim().is_empty())
            .unwrap();
        let (der_hex, fp_hex) = line.split_once('|').unwrap();
        let der = from_hex(der_hex).unwrap();
        assert_eq!(to_hex(&fingerprint(&der)), fp_hex);
        let cert = CertificateDer::from(der);
        rustls::server::ParsedCertificate::try_from(&cert).expect("webpki parses the certificate");
    }
}
