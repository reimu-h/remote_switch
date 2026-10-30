// SPDX-License-Identifier: MIT

//! Authenticated plain-text protocol for remote switch commands.
//!
//! Every message is one short ASCII line, so the same format works over UDP,
//! the 1NCE API, SMS, or the debug shell:
//!
//! ```text
//! request:  <command> <counter> <tag>    e.g. "on 1761840000123 9f3c2a7d10b4e6f1"
//! reply:    <status> <counter> <tag>     e.g. "on 1761840000123 03ab5c9e27f8d1c4"
//! ```
//!
//! * `command` is `on`, `off` or `status`. Input is case-insensitive.
//! * `counter` is a decimal `u64` that must increase with every request. It
//!   provides replay protection. Clients use the current Unix time in
//!   milliseconds.
//! * `tag` is the first [`TAG_LEN`] bytes of
//!   `HMAC-SHA256(key, domain || "<command> <counter>")`, encoded as hex.
//!   Requests and replies use different domain prefixes, so a captured reply
//!   can never be replayed as a request.
//!
//! A request plus its reply costs about 60 bytes of payload. That is roughly
//! 120 bytes on the wire including IPv4 and UDP headers.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Length of the pre-shared key in bytes.
pub const KEY_LEN: usize = 32;

/// Length of the truncated HMAC tag in bytes (encoded as twice as many hex characters).
pub const TAG_LEN: usize = 8;

/// Maximum accepted message length in bytes. Must match `REMOTE_MESSAGE_MAX_LEN` in
/// `src/board_io.h`.
pub const MAX_MESSAGE_LEN: usize = 64;

const REQUEST_DOMAIN: &[u8] = b"rs1 req ";
const REPLY_DOMAIN: &[u8] = b"rs1 rsp ";
const MAX_COUNTER_DIGITS: usize = 20;

type HmacSha256 = Hmac<Sha256>;

/// Pre-shared HMAC key used to authenticate requests and replies.
#[derive(Clone)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// Wraps raw key bytes.
    #[must_use]
    pub const fn new(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// Parses a key from exactly `2 * KEY_LEN` hex characters (either case).
    #[must_use]
    pub fn from_hex(hex: &str) -> Option<Self> {
        let mut bytes = [0u8; KEY_LEN];
        decode_hex(hex.as_bytes(), &mut bytes)?;
        Some(Self(bytes))
    }

    fn mac(&self, domain: &[u8], word: &str, counter: u64) -> HmacSha256 {
        let mut digits = [0u8; MAX_COUNTER_DIGITS];
        // `new_from_slice` only fails for invalid key lengths, and HMAC accepts any length.
        let mut mac =
            HmacSha256::new_from_slice(&self.0).unwrap_or_else(|_| unreachable!("HMAC key"));
        mac.update(domain);
        mac.update(word.as_bytes());
        mac.update(b" ");
        mac.update(format_counter(counter, &mut digits));
        mac
    }

    fn tag(&self, domain: &[u8], word: &str, counter: u64) -> [u8; TAG_LEN] {
        let digest = self.mac(domain, word, counter).finalize().into_bytes();
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&digest[..TAG_LEN]);
        tag
    }
}

/// Commands a remote client can send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteCommand {
    /// Turn the switch on.
    On,
    /// Turn the switch off.
    Off,
    /// Report the switch state without changing it.
    Status,
}

impl RemoteCommand {
    /// Canonical wire spelling, which is also what the tag authenticates.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Status => "status",
        }
    }

    fn parse(token: &[u8]) -> Option<Self> {
        [Self::On, Self::Off, Self::Status]
            .into_iter()
            .find(|cmd| token.eq_ignore_ascii_case(cmd.as_str().as_bytes()))
    }
}

/// An authenticated request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// Requested operation.
    pub command: RemoteCommand,
    /// Client counter. The replay guard requires it to increase.
    pub counter: u64,
}

/// Reasons a message is rejected. No reply is sent for any of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    /// Message is longer than [`MAX_MESSAGE_LEN`].
    TooLong,
    /// Message is not exactly three space-separated ASCII tokens.
    Malformed,
    /// First token is not a known command.
    UnknownCommand,
    /// Counter is not a canonical decimal `u64` greater than zero.
    BadCounter,
    /// Tag is malformed or does not match the message.
    BadTag,
}

/// Parses `message` and verifies its tag against `key` in constant time.
pub fn verify_request(key: &Key, message: &[u8]) -> Result<Request, RequestError> {
    if message.len() > MAX_MESSAGE_LEN {
        return Err(RequestError::TooLong);
    }
    let message = message.trim_ascii();
    if !message.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        return Err(RequestError::Malformed);
    }

    let mut tokens = message.split(|b| *b == b' ');
    let (Some(command), Some(counter), Some(tag), None) =
        (tokens.next(), tokens.next(), tokens.next(), tokens.next())
    else {
        return Err(RequestError::Malformed);
    };

    let command = RemoteCommand::parse(command).ok_or(RequestError::UnknownCommand)?;
    let counter = parse_counter(counter).ok_or(RequestError::BadCounter)?;
    let mut tag_bytes = [0u8; TAG_LEN];
    decode_hex(tag, &mut tag_bytes).ok_or(RequestError::BadTag)?;

    key.mac(REQUEST_DOMAIN, command.as_str(), counter)
        .verify_truncated_left(&tag_bytes)
        .map_err(|_| RequestError::BadTag)?;
    Ok(Request { command, counter })
}

/// Status values the device sends back to an authenticated client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyStatus {
    /// Switch is (now) on.
    On,
    /// Switch is (now) off.
    Off,
    /// Counter was not newer than the last accepted one. The reply carries the last
    /// accepted counter so the client can resynchronize.
    Stale,
    /// Switch output hardware is unavailable; state is forced off.
    Unavailable,
    /// Command queue was full; the command was not applied.
    Busy,
}

impl ReplyStatus {
    /// Canonical wire spelling, which is also what the tag authenticates.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Stale => "stale",
            Self::Unavailable => "unavailable",
            Self::Busy => "busy",
        }
    }
}

/// Writes an authenticated reply into `out`, returning its length, or `None` if
/// `out` is too small.
pub fn encode_reply(key: &Key, status: ReplyStatus, counter: u64, out: &mut [u8]) -> Option<usize> {
    encode(key, REPLY_DOMAIN, status.as_str(), counter, out)
}

/// Writes an authenticated request into `out`, returning its length, or `None` if
/// `out` is too small. The firmware never sends requests. This exists to test the
/// wire format against `tools/rsctl.py`.
pub fn encode_request(
    key: &Key,
    command: RemoteCommand,
    counter: u64,
    out: &mut [u8],
) -> Option<usize> {
    encode(key, REQUEST_DOMAIN, command.as_str(), counter, out)
}

fn encode(key: &Key, domain: &[u8], word: &str, counter: u64, out: &mut [u8]) -> Option<usize> {
    let mut digits = [0u8; MAX_COUNTER_DIGITS];
    let counter_text = format_counter(counter, &mut digits);
    let tag = key.tag(domain, word, counter);

    let mut writer = Writer { out, len: 0 };
    writer.push(word.as_bytes())?;
    writer.push(b" ")?;
    writer.push(counter_text)?;
    writer.push(b" ")?;
    for byte in tag {
        writer.push(&[hex_digit(byte >> 4), hex_digit(byte & 0x0f)])?;
    }
    Some(writer.len)
}

struct Writer<'a> {
    out: &'a mut [u8],
    len: usize,
}

impl Writer<'_> {
    fn push(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.out.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }
}

/// Parses a canonical decimal counter: 1-20 digits, no leading zeros, non-zero, fits in `u64`.
fn parse_counter(token: &[u8]) -> Option<u64> {
    if token.is_empty() || token.len() > MAX_COUNTER_DIGITS || token[0] == b'0' {
        return None;
    }
    token.iter().try_fold(0u64, |acc, &b| {
        if !b.is_ascii_digit() {
            return None;
        }
        acc.checked_mul(10)?.checked_add(u64::from(b - b'0'))
    })
}

fn format_counter(mut value: u64, buf: &mut [u8; MAX_COUNTER_DIGITS]) -> &[u8] {
    let mut start = buf.len();
    loop {
        start -= 1;
        // `value % 10` is always < 10, so the narrowing cast is lossless.
        buf[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    &buf[start..]
}

fn decode_hex(hex: &[u8], out: &mut [u8]) -> Option<()> {
    if hex.len() != out.len() * 2 {
        return None;
    }
    for (byte, pair) in out.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = (hex_value(pair[0])? << 4) | hex_value(pair[1])?;
    }
    Some(())
}

const fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

const fn hex_digit(nibble: u8) -> u8 {
    match nibble {
        0..=9 => b'0' + nibble,
        _ => b'a' + nibble - 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    fn key() -> Key {
        Key::from_hex(KEY_HEX).unwrap()
    }

    fn request(command: RemoteCommand, counter: u64) -> Vec<u8> {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_request(&key(), command, counter, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    #[test]
    fn known_answers_match_reference_hmac() {
        // Reference: Python `hmac.new(key, b"rs1 req on 1761840000123", "sha256").hexdigest()[:16]`.
        assert_eq!(
            request(RemoteCommand::On, 1_761_840_000_123),
            b"on 1761840000123 16d82a013add92ef".to_vec()
        );
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_reply(&key(), ReplyStatus::On, 1_761_840_000_123, &mut buf).unwrap();
        assert_eq!(&buf[..len], b"on 1761840000123 50d3e76858638e40");
    }

    #[test]
    fn round_trips_all_commands() {
        for command in [RemoteCommand::On, RemoteCommand::Off, RemoteCommand::Status] {
            for counter in [1, 42, 1_761_840_000_123, u64::MAX] {
                assert_eq!(
                    verify_request(&key(), &request(command, counter)),
                    Ok(Request { command, counter })
                );
            }
        }
    }

    #[test]
    fn accepts_case_insensitive_input_and_surrounding_whitespace() {
        let canonical = String::from_utf8(request(RemoteCommand::Off, 7)).unwrap();
        let shouted = format!(" \r\n{}\r\n", canonical.to_ascii_uppercase());
        assert_eq!(
            verify_request(&key(), shouted.as_bytes()),
            Ok(Request {
                command: RemoteCommand::Off,
                counter: 7
            })
        );
    }

    #[test]
    fn rejects_wrong_key_and_tampering() {
        let other = Key::new([0x55; KEY_LEN]);
        let msg = request(RemoteCommand::On, 9);
        assert_eq!(verify_request(&other, &msg), Err(RequestError::BadTag));

        let text = String::from_utf8(msg).unwrap();
        let tampered_cmd = text.replacen("on", "off", 1);
        assert_eq!(
            verify_request(&key(), tampered_cmd.as_bytes()),
            Err(RequestError::BadTag)
        );
        let tampered_ctr = text.replacen(" 9 ", " 10 ", 1);
        assert_eq!(
            verify_request(&key(), tampered_ctr.as_bytes()),
            Err(RequestError::BadTag)
        );
    }

    #[test]
    fn reply_cannot_be_replayed_as_request() {
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_reply(&key(), ReplyStatus::On, 77, &mut buf).unwrap();
        assert_eq!(
            verify_request(&key(), &buf[..len]),
            Err(RequestError::BadTag)
        );
    }

    #[test]
    fn rejects_malformed_messages() {
        let k = key();
        let cases: &[(&[u8], RequestError)] = &[
            (b"", RequestError::Malformed),
            (b"on", RequestError::Malformed),
            (b"on 1", RequestError::Malformed),
            (b"on 1 0011223344556677 extra", RequestError::Malformed),
            (b"on  1 0011223344556677", RequestError::Malformed),
            (b"on\t1\t0011223344556677", RequestError::Malformed),
            (b"on 1 0011223344556677\x00", RequestError::Malformed),
            (b"toggle 1 0011223344556677", RequestError::UnknownCommand),
            (b"on 0 0011223344556677", RequestError::BadCounter),
            (b"on 01 0011223344556677", RequestError::BadCounter),
            (b"on -1 0011223344556677", RequestError::BadCounter),
            (
                b"on 18446744073709551616 0011223344556677",
                RequestError::BadCounter,
            ),
            (b"on 1 001122334455667", RequestError::BadTag),
            (b"on 1 00112233445566778", RequestError::BadTag),
            (b"on 1 00112233445566zz", RequestError::BadTag),
            (b"on 1 0011223344556677", RequestError::BadTag),
        ];
        for (msg, err) in cases {
            assert_eq!(verify_request(&k, msg), Err(*err), "message {msg:?}");
        }
        assert_eq!(
            verify_request(&k, &[b'a'; MAX_MESSAGE_LEN + 1]),
            Err(RequestError::TooLong)
        );
    }

    #[test]
    fn encoders_fit_max_message_len_and_reject_small_buffers() {
        // Longest possible reply: longest status word and a 20-digit counter.
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        let len = encode_reply(&key(), ReplyStatus::Unavailable, u64::MAX, &mut buf).unwrap();
        let prefix = format!("unavailable {} ", u64::MAX);
        assert_eq!(len, prefix.len() + 2 * TAG_LEN);
        assert!(len <= MAX_MESSAGE_LEN);
        assert!(buf[..len].starts_with(prefix.as_bytes()));

        assert_eq!(
            encode_reply(&key(), ReplyStatus::On, 1, &mut buf[..10]),
            None
        );
        assert_eq!(
            encode_request(&key(), RemoteCommand::Status, 1, &mut buf[..0]),
            None
        );
    }

    #[test]
    fn key_parsing_requires_exact_hex_length() {
        assert!(Key::from_hex(KEY_HEX).is_some());
        assert!(Key::from_hex(&KEY_HEX.to_ascii_uppercase()).is_some());
        assert!(Key::from_hex("").is_none());
        assert!(Key::from_hex(&KEY_HEX[..62]).is_none());
        assert!(Key::from_hex(&format!("{KEY_HEX}00")).is_none());
        assert!(Key::from_hex(&KEY_HEX.replace('0', "g")).is_none());
    }

    #[test]
    fn counter_formatting_round_trips() {
        let mut buf = [0u8; MAX_COUNTER_DIGITS];
        for value in [1, 9, 10, 1_000_000, u64::MAX] {
            let text = format_counter(value, &mut buf);
            assert_eq!(parse_counter(text), Some(value));
        }
        assert_eq!(format_counter(0, &mut buf), b"0");
    }
}
