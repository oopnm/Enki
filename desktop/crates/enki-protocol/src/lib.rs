//! Enki Link wire format, version 1.
//!
//! Every TCP connection starts with an 8-byte [`Preamble`], followed by a short authentication exchange
//! (see [`auth_mac`]). A media connection then carries frames: a 16-byte [`FrameHeader`] followed by `len`
//! payload bytes. A control connection carries length-prefixed JSON messages (handled elsewhere).
//! All integers are big-endian. See `docs/protocol.md`.

pub mod control;

use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const MAGIC: [u8; 4] = *b"ENKI";
pub const VERSION: u8 = 1;
pub const PREAMBLE_LEN: usize = 8;
pub const FRAME_HEADER_LEN: usize = 16;
/// Upper bound for one media payload; larger values are rejected as corrupt.
pub const MAX_PAYLOAD: u32 = 16 * 1024 * 1024;
/// Session token: 32 random bytes the PC hands to the phone over adb, never over the Enki Link socket.
pub const TOKEN_LEN: usize = 32;
/// Random challenge the phone sends right after the preamble exchange.
pub const NONCE_LEN: usize = 16;
pub const MAC_LEN: usize = 32;
const AUTH_CONTEXT: &[u8] = b"ENKI-AUTH-1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    BadMagic,
    UnsupportedVersion(u8),
    UnknownChannel(u8),
    UnknownKind(u8),
    PayloadTooLarge(u32),
    Reserved,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::BadMagic => write!(f, "not an Enki Link stream (bad magic)"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            Error::UnknownChannel(c) => write!(f, "unknown channel {c}"),
            Error::UnknownKind(k) => write!(f, "unknown frame kind {k}"),
            Error::PayloadTooLarge(n) => write!(f, "payload of {n} bytes exceeds the limit"),
            Error::Reserved => write!(f, "reserved bytes must be zero"),
        }
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Channel {
    Media = 0,
    Control = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preamble {
    pub version: u8,
    pub channel: Channel,
}

impl Preamble {
    pub fn new(channel: Channel) -> Self {
        Self {
            version: VERSION,
            channel,
        }
    }

    pub fn encode(&self) -> [u8; PREAMBLE_LEN] {
        let mut b = [0u8; PREAMBLE_LEN];
        b[..4].copy_from_slice(&MAGIC);
        b[4] = self.version;
        b[5] = self.channel as u8;
        b
    }

    pub fn decode(b: &[u8; PREAMBLE_LEN]) -> Result<Self, Error> {
        if b[..4] != MAGIC {
            return Err(Error::BadMagic);
        }
        if b[4] != VERSION {
            return Err(Error::UnsupportedVersion(b[4]));
        }
        if b[6] != 0 || b[7] != 0 {
            return Err(Error::Reserved);
        }
        let channel = match b[5] {
            0 => Channel::Media,
            1 => Channel::Control,
            c => return Err(Error::UnknownChannel(c)),
        };
        Ok(Self {
            version: b[4],
            channel,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    VideoConfig = 0,
    VideoKey = 1,
    VideoDelta = 2,
    AudioConfig = 3,
    AudioFrame = 4,
}

impl Kind {
    fn from_u8(v: u8) -> Result<Self, Error> {
        Ok(match v {
            0 => Kind::VideoConfig,
            1 => Kind::VideoKey,
            2 => Kind::VideoDelta,
            3 => Kind::AudioConfig,
            4 => Kind::AudioFrame,
            k => return Err(Error::UnknownKind(k)),
        })
    }
}

/// Flag bit: the frame follows a gap (dropped data); the decoder should resync.
pub const FLAG_DISCONTINUITY: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub stream_id: u8,
    pub kind: Kind,
    pub flags: u8,
    pub len: u32,
    pub pts_us: u64,
}

impl FrameHeader {
    pub fn encode(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut b = [0u8; FRAME_HEADER_LEN];
        b[0] = self.stream_id;
        b[1] = self.kind as u8;
        b[2] = self.flags;
        b[4..8].copy_from_slice(&self.len.to_be_bytes());
        b[8..16].copy_from_slice(&self.pts_us.to_be_bytes());
        b
    }

    pub fn decode(b: &[u8; FRAME_HEADER_LEN]) -> Result<Self, Error> {
        if b[3] != 0 {
            return Err(Error::Reserved);
        }
        let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        if len > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge(len));
        }
        Ok(Self {
            stream_id: b[0],
            kind: Kind::from_u8(b[1])?,
            flags: b[2],
            len,
            pts_us: u64::from_be_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]),
        })
    }
}

/// Answer to the phone's challenge: `HMAC-SHA256(token, "ENKI-AUTH-1" || channel || nonce)`.
/// The channel is part of the message, so an answer for one channel cannot be replayed on the other.
pub fn auth_mac(
    token: &[u8; TOKEN_LEN],
    channel: Channel,
    nonce: &[u8; NONCE_LEN],
) -> [u8; MAC_LEN] {
    let mut mac =
        <Hmac<Sha256> as Mac>::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(AUTH_CONTEXT);
    mac.update(&[channel as u8]);
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

/// The phone's verdict, one byte sent after it has read the PC's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AuthStatus {
    Ok = 0,
    /// The answer did not match the phone's token.
    BadToken = 1,
    /// The phone has no token yet (the PC has not provisioned it, or the app restarted).
    NotProvisioned = 2,
}

impl AuthStatus {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(AuthStatus::Ok),
            1 => Some(AuthStatus::BadToken),
            2 => Some(AuthStatus::NotProvisioned),
            _ => None,
        }
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses an even-length hex string; `None` for anything else.
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        from_hex(s).unwrap()
    }

    /// Shared vectors: `protocol/vectors/frame_header.txt`, also used by the Android side.
    /// Line format: name|stream|kind|flags|len|pts_us|hex
    #[test]
    fn shared_vectors() {
        let text = include_str!("../../../../protocol/vectors/frame_header.txt");
        let mut n = 0;
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let f: Vec<&str> = line.split('|').collect();
            let hex: [u8; FRAME_HEADER_LEN] = unhex(f[6]).try_into().unwrap();
            let h = FrameHeader {
                stream_id: f[1].parse().unwrap(),
                kind: Kind::from_u8(f[2].parse().unwrap()).unwrap(),
                flags: f[3].parse().unwrap(),
                len: f[4].parse().unwrap(),
                pts_us: f[5].parse().unwrap(),
            };
            assert_eq!(h.encode(), hex, "{}", f[0]);
            assert_eq!(FrameHeader::decode(&hex).unwrap(), h, "{}", f[0]);
            n += 1;
        }
        assert!(n >= 3);
    }

    #[test]
    fn preamble_roundtrip() {
        for ch in [Channel::Media, Channel::Control] {
            let p = Preamble::new(ch);
            assert_eq!(Preamble::decode(&p.encode()).unwrap(), p);
        }
        assert_eq!(&Preamble::new(Channel::Media).encode(), b"ENKI\x01\0\0\0");
    }

    /// Shared vectors: `protocol/vectors/auth.txt`, also used by the Android side.
    /// Line format: name|token_hex|channel|nonce_hex|mac_hex
    #[test]
    fn shared_auth_vectors() {
        let text = include_str!("../../../../protocol/vectors/auth.txt");
        let mut n = 0;
        for line in text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        {
            let f: Vec<&str> = line.split('|').collect();
            let token: [u8; TOKEN_LEN] = unhex(f[1]).try_into().unwrap();
            let channel = match f[2] {
                "0" => Channel::Media,
                _ => Channel::Control,
            };
            let nonce: [u8; NONCE_LEN] = unhex(f[3]).try_into().unwrap();
            assert_eq!(to_hex(&auth_mac(&token, channel, &nonce)), f[4], "{}", f[0]);
            n += 1;
        }
        assert!(n >= 3);
    }

    #[test]
    fn auth_answer_is_bound_to_token_channel_and_nonce() {
        let token = [7u8; TOKEN_LEN];
        let nonce = [1u8; NONCE_LEN];
        let base = auth_mac(&token, Channel::Control, &nonce);
        assert_ne!(
            base,
            auth_mac(&token, Channel::Media, &nonce),
            "no replay across channels"
        );
        assert_ne!(base, auth_mac(&[8u8; TOKEN_LEN], Channel::Control, &nonce));
        assert_ne!(base, auth_mac(&token, Channel::Control, &[2u8; NONCE_LEN]));
    }

    #[test]
    fn hex_and_status_helpers() {
        assert_eq!(to_hex(&[0, 0xab, 0xff]), "00abff");
        assert_eq!(from_hex("00abFF"), Some(vec![0, 0xab, 0xff]));
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(AuthStatus::from_u8(2), Some(AuthStatus::NotProvisioned));
        assert_eq!(AuthStatus::from_u8(9), None);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Preamble::decode(b"HTTP/1.1"), Err(Error::BadMagic));
        assert_eq!(
            Preamble::decode(b"ENKI\x09\0\0\0"),
            Err(Error::UnsupportedVersion(9))
        );
        assert_eq!(
            Preamble::decode(b"ENKI\0\0\0\0"),
            Err(Error::UnsupportedVersion(0)),
            "an old phone app"
        );
        assert_eq!(
            Preamble::decode(b"ENKI\x01\x07\0\0"),
            Err(Error::UnknownChannel(7))
        );
        let mut h = FrameHeader {
            stream_id: 0,
            kind: Kind::VideoKey,
            flags: 0,
            len: 1,
            pts_us: 0,
        }
        .encode();
        h[1] = 99;
        assert_eq!(FrameHeader::decode(&h), Err(Error::UnknownKind(99)));
        h[1] = 1;
        h[4..8].copy_from_slice(&(MAX_PAYLOAD + 1).to_be_bytes());
        assert_eq!(
            FrameHeader::decode(&h),
            Err(Error::PayloadTooLarge(MAX_PAYLOAD + 1))
        );
        h[4..8].copy_from_slice(&1u32.to_be_bytes());
        h[3] = 1;
        assert_eq!(FrameHeader::decode(&h), Err(Error::Reserved));
    }
}
