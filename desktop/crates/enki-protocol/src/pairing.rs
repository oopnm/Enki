//! Wi-Fi pairing helpers (docs/protocol.md, "Wi-Fi"): the TLS-bound answer to the phone's challenge and the
//! pairing code that becomes a one-time key.

use crate::{Channel, MAC_LEN, NONCE_LEN};
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// SHA-256 of the phone's certificate (DER). The PC pins it after pairing.
pub const FINGERPRINT_LEN: usize = 32;
const TLS_CONTEXT: &[u8] = b"ENKI-AUTH-2";
const PAIRING_SALT: &[u8] = b"ENKI-PAIR-1";
const PAIRING_ROUNDS: u32 = 20_000;
/// Symbols of a pairing code: digits and capitals without I, L, O, U (32 of them, so 5 bits each).
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 10;

/// Answer to the phone's challenge on a TLS connection:
/// `HMAC-SHA256(key, "ENKI-AUTH-2" || channel || fingerprint || nonce)`.
///
/// The fingerprint is the one the PC saw in the phone's certificate. A machine in the middle shows its own
/// certificate to the PC, so the PC's answer contains the wrong fingerprint and the real phone refuses it.
pub fn auth_mac_tls(
    key: &[u8; 32],
    channel: Channel,
    fingerprint: &[u8; FINGERPRINT_LEN],
    nonce: &[u8; NONCE_LEN],
) -> [u8; MAC_LEN] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(TLS_CONTEXT);
    mac.update(&[channel as u8]);
    mac.update(fingerprint);
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

/// Upper-cases the code, drops spaces and dashes, and reads O as 0 and I or L as 1 (so a misread letter still
/// works). Returns `None` if anything else is left that is not in [`CODE_ALPHABET`] or the length is wrong.
pub fn normalize_code(code: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in code.chars() {
        let c = match c.to_ascii_uppercase() {
            ' ' | '-' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            other => other,
        };
        if !c.is_ascii() || !CODE_ALPHABET.contains(&(c as u8)) {
            return None;
        }
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

/// Builds a code from `CODE_LEN` random bytes (each byte contributes its low 5 bits; 256 is a multiple of 32, so
/// the symbols are uniform). Shown as `ABCDE-FGHJK`.
pub fn code_from_random(random: &[u8; CODE_LEN]) -> String {
    let mut s = String::with_capacity(CODE_LEN + 1);
    for (i, b) in random.iter().enumerate() {
        if i == CODE_LEN / 2 {
            s.push('-');
        }
        s.push(CODE_ALPHABET[(*b & 31) as usize] as char);
    }
    s
}

/// The one-time key both sides derive from a pairing code: PBKDF2-HMAC-SHA256 with a fixed salt and 20 000
/// rounds. The code has 50 bits; the rounds make guessing from a captured answer slow, and the phone also
/// refuses an offer after a few wrong answers. Returns `None` for a malformed code.
pub fn pairing_key(code: &str) -> Option<[u8; 32]> {
    let normalized = normalize_code(code)?;
    Some(pbkdf2_sha256(
        normalized.as_bytes(),
        PAIRING_SALT,
        PAIRING_ROUNDS,
    ))
}

/// One 32-byte block of PBKDF2 (RFC 8018) with HMAC-SHA256.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], rounds: u32) -> [u8; 32] {
    let prf = <Hmac<Sha256> as Mac>::new_from_slice(password).expect("HMAC accepts any key length");
    let mut first = prf.clone();
    first.update(salt);
    first.update(&1u32.to_be_bytes());
    let mut u: [u8; 32] = first.finalize().into_bytes().into();
    let mut out = u;
    for _ in 1..rounds {
        let mut m = prf.clone();
        m.update(&u);
        u = m.finalize().into_bytes().into();
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{from_hex, to_hex};

    fn lines(text: &'static str) -> impl Iterator<Item = Vec<&'static str>> {
        text.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| l.split('|').collect())
    }

    #[test]
    fn shared_tls_auth_vectors() {
        let mut n = 0;
        for f in lines(include_str!("../../../../protocol/vectors/auth_tls.txt")) {
            let key: [u8; 32] = from_hex(f[1]).unwrap().try_into().unwrap();
            let channel = if f[2] == "0" {
                Channel::Media
            } else {
                Channel::Control
            };
            let fp: [u8; 32] = from_hex(f[3]).unwrap().try_into().unwrap();
            let nonce: [u8; 16] = from_hex(f[4]).unwrap().try_into().unwrap();
            assert_eq!(
                to_hex(&auth_mac_tls(&key, channel, &fp, &nonce)),
                f[5],
                "{}",
                f[0]
            );
            n += 1;
        }
        assert!(n >= 2);
    }

    #[test]
    fn tls_answer_depends_on_the_fingerprint() {
        let key = [3u8; 32];
        let nonce = [9u8; 16];
        let a = auth_mac_tls(&key, Channel::Control, &[1u8; 32], &nonce);
        assert_ne!(a, auth_mac_tls(&key, Channel::Control, &[2u8; 32], &nonce));
        assert_ne!(
            a,
            crate::auth_mac(&key, Channel::Control, &nonce),
            "not the plain-socket answer"
        );
    }

    #[test]
    fn shared_pairing_vectors() {
        let mut n = 0;
        for f in lines(include_str!("../../../../protocol/vectors/pairing.txt")) {
            assert_eq!(normalize_code(f[0]).as_deref(), Some(f[1]), "{}", f[0]);
            assert_eq!(to_hex(&pairing_key(f[0]).unwrap()), f[2], "{}", f[0]);
            n += 1;
        }
        assert!(n >= 4);
    }

    #[test]
    fn codes_are_checked_and_formatted() {
        assert_eq!(normalize_code("abcde-fghjk").as_deref(), Some("ABCDEFGHJK"));
        assert_eq!(normalize_code("ABCDE-FGHJ"), None, "too short");
        assert_eq!(normalize_code("ABCDE-FGHJKM"), None, "too long");
        assert_eq!(normalize_code("ABCDE-FGHJU"), None, "U is not a symbol");
        assert_eq!(normalize_code("ABCDE-FGHJé"), None);
        assert_eq!(
            code_from_random(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]),
            "01234-56789"
        );
        assert_eq!(code_from_random(&[255; 10]), "ZZZZZ-ZZZZZ");
        let code = code_from_random(&[31, 17, 200, 3, 4, 5, 6, 7, 8, 99]);
        assert!(normalize_code(&code).is_some(), "{code}");
        assert_eq!(pairing_key("ABCDE-FGHJ"), None);
    }
}
