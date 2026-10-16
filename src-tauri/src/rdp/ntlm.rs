//! NTLMv2 ([MS-NLMP]), as much as an RD Gateway's HTTP sign-in needs: the three messages, no signing or sealing
//! afterwards (ported from tabby-rdp's ntlm.ts). Written out rather than taken from a crate: it is small, and the
//! password passes through it. MD4 and HMAC-MD5 are RustCrypto's; the rest is ring.
use hmac::{KeyInit, Mac};

const SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

const NEGOTIATE_UNICODE: u32 = 0x0000_0001;
const REQUEST_TARGET: u32 = 0x0000_0004;
const NEGOTIATE_NTLM: u32 = 0x0000_0200;
const NEGOTIATE_ALWAYS_SIGN: u32 = 0x0000_8000;
const NEGOTIATE_EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
const NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;
const NEGOTIATE_VERSION: u32 = 0x0200_0000;
const NEGOTIATE_128: u32 = 0x2000_0000;
const NEGOTIATE_56: u32 = 0x8000_0000;

// AV pairs of the server's target information.
const AV_EOL: u16 = 0;
const AV_FLAGS: u16 = 6;
const AV_TIMESTAMP: u16 = 7;
const AV_TARGET_NAME: u16 = 9;
const AV_CHANNEL_BINDINGS: u16 = 10;
/// MsvAvFlags: the authenticate message carries a message integrity code.
const AV_FLAG_MIC: u32 = 0x2;

pub(super) fn md4(data: &[u8]) -> [u8; 16] {
    use md4::Digest;
    md4::Md4::digest(data).into()
}

pub(super) fn hmac_md5(key: &[u8], data: &[&[u8]]) -> [u8; 16] {
    let mut mac = hmac::Hmac::<md5::Md5>::new_from_slice(key).expect("HMAC takes any key");
    for part in data {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

fn md5(data: &[&[u8]]) -> [u8; 16] {
    use md5::Digest;
    let mut hash = md5::Md5::new();
    for part in data {
        hash.update(part);
    }
    hash.finalize().into()
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; N];
    ring::rand::SystemRandom::new().fill(&mut bytes).expect("randomness");
    bytes
}

fn u16_at(b: &[u8], at: usize) -> u16 { u16::from_le_bytes([b[at], b[at + 1]]) }
fn u32_at(b: &[u8], at: usize) -> u32 { u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) }

/// NTOWFv2: what the password, user and domain come to.
pub(super) fn ntlm_v2_hash(username: &str, domain: &str, password: &str) -> [u8; 16] {
    hmac_md5(&md4(&utf16(password)), &[&utf16(&(username.to_uppercase() + domain))])
}

/// The first message: what this client can do.
pub(super) fn negotiate_message() -> Vec<u8> {
    let mut message = vec![0u8; 32];
    message[..8].copy_from_slice(SIGNATURE);
    message[8..12].copy_from_slice(&1u32.to_le_bytes());
    let flags = NEGOTIATE_UNICODE | REQUEST_TARGET | NEGOTIATE_NTLM | NEGOTIATE_ALWAYS_SIGN | NEGOTIATE_EXTENDED_SESSIONSECURITY
        | NEGOTIATE_128 | NEGOTIATE_56;
    message[12..16].copy_from_slice(&flags.to_le_bytes());
    // Domain and workstation: none given (their fields point at the end, with no length).
    message[20..24].copy_from_slice(&32u32.to_le_bytes());
    message[28..32].copy_from_slice(&32u32.to_le_bytes());
    message
}

/// The server's challenge message, read.
#[derive(Debug)]
pub(super) struct Challenge {
    pub flags: u32,
    pub challenge: [u8; 8],
    pub target_info: Vec<u8>,
}

pub(super) fn parse_challenge(message: &[u8]) -> Result<Challenge, String> {
    if message.len() < 48 || &message[..8] != SIGNATURE || u32_at(message, 8) != 2 {
        return Err("not an NTLM challenge".into());
    }
    let flags = u32_at(message, 20);
    let (length, offset) = (u16_at(message, 40) as usize, u32_at(message, 44) as usize);
    if offset.checked_add(length).is_none_or(|end| end > message.len()) {
        return Err("the NTLM challenge's target information is cut short".into());
    }
    let mut challenge = [0u8; 8];
    challenge.copy_from_slice(&message[24..32]);
    let target_info = if flags & NEGOTIATE_TARGET_INFO != 0 { message[offset..offset + length].to_vec() } else { Vec::new() };
    Ok(Challenge { flags, challenge, target_info })
}

/// The target information's AV pairs, as (id, value), up to the end marker.
fn av_pairs(info: &[u8]) -> Vec<(u16, &[u8])> {
    let mut pairs = Vec::new();
    let mut at = 0;
    while at + 4 <= info.len() {
        let (id, length) = (u16_at(info, at), u16_at(info, at + 2) as usize);
        if id == AV_EOL || at + 4 + length > info.len() {
            break;
        }
        pairs.push((id, &info[at + 4..at + 4 + length]));
        at += 4 + length;
    }
    pairs
}

fn pair(id: u16, value: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(value.len() as u16).to_le_bytes());
    out.extend_from_slice(value);
}

/// The channel binding's hash ([MS-NLMP] 3.1.5.1.2): MD5 over a gss_channel_bindings_struct with no addresses and the
/// binding as its application data.
pub(super) fn channel_binding_hash(binding: &[u8]) -> [u8; 16] {
    let mut head = [0u8; 20];
    head[16..20].copy_from_slice(&(binding.len() as u32).to_le_bytes());
    md5(&[&head, binding])
}

/// The target information as the client's response carries it back: the server's pairs, and the client's own (an
/// integrity code follows when `mic`; which TLS connection and service the sign-in is for).
fn client_target_info(info: &[u8], mic: bool, channel_binding: Option<&[u8]>, target_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut flags = if mic { AV_FLAG_MIC } else { 0 };
    for (id, value) in av_pairs(info) {
        if id == AV_FLAGS && value.len() == 4 {
            flags |= u32_at(value, 0);
        } else if id != AV_CHANNEL_BINDINGS && id != AV_TARGET_NAME {
            pair(id, value, &mut out);
        }
    }
    if flags != 0 {
        pair(AV_FLAGS, &flags.to_le_bytes(), &mut out);
    }
    // No binding to give: zeros, as Windows sends them (a server that insists on one refuses those).
    pair(AV_CHANNEL_BINDINGS, &channel_binding.map(channel_binding_hash).unwrap_or_default(), &mut out);
    pair(AV_TARGET_NAME, &utf16(target_name), &mut out);
    out.extend_from_slice(&[0; 4]);
    out
}

/// Windows time (100 ns since 1601) of now.
fn now() -> [u8; 8] {
    let since_1970 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    ((since_1970.as_millis() as u64 + 11_644_473_600_000) * 10_000).to_le_bytes()
}

/// The NTLMv2 response to a challenge ([MS-NLMP] 3.3.2): the proof with the blob it covers, and the session's base key.
pub(super) fn ntlm_v2_response(hash: &[u8; 16], server_challenge: &[u8; 8], target_info: &[u8], time: [u8; 8], client_challenge: [u8; 8]) -> (Vec<u8>, [u8; 16]) {
    let mut blob = vec![1, 1, 0, 0, 0, 0, 0, 0];
    blob.extend_from_slice(&time);
    blob.extend_from_slice(&client_challenge);
    blob.extend_from_slice(&[0; 4]);
    blob.extend_from_slice(target_info);
    blob.extend_from_slice(&[0; 4]);
    let proof = hmac_md5(hash, &[server_challenge, &blob]);
    let session_key = hmac_md5(hash, &[&proof]);
    ([proof.as_slice(), &blob].concat(), session_key)
}

/// Who signs in, for which TLS connection and service.
pub(super) struct Account<'a> {
    /// `user`, `DOMAIN\user` or `user@domain` (the latter two carry their domain).
    pub username: &'a str,
    pub domain: &'a str,
    pub password: &'a str,
    pub workstation: &'a str,
    /// "tls-server-end-point:" and the certificate's hash: servers set up for extended protection (an RD Gateway, by
    /// default) refuse a sign-in without it.
    pub channel_binding: Option<&'a [u8]>,
    /// The service signed in to (`HTTP/host`).
    pub target_name: &'a str,
}

/// The third message: the answer to the server's challenge, for this account. `negotiate` and `challenge` are the two
/// messages before it, which its integrity code covers.
pub(super) fn authenticate_message(negotiate: &[u8], challenge: &[u8], account: &Account<'_>) -> Result<Vec<u8>, String> {
    authenticate_with(negotiate, challenge, account, random::<8>())
}

fn authenticate_with(negotiate: &[u8], challenge: &[u8], account: &Account<'_>, client_challenge: [u8; 8]) -> Result<Vec<u8>, String> {
    let server = parse_challenge(challenge)?;
    let (domain, username) = match account.username.split_once('\\') {
        Some((domain, user)) if !domain.is_empty() && !user.is_empty() => (domain, user),
        _ => (account.domain, account.username),
    };
    // The server says when it is (a timestamp among its pairs) on every Windows since Vista: the response then carries
    // an integrity code, and the LM response is left empty.
    let timestamp = av_pairs(&server.target_info).into_iter().find(|(id, value)| *id == AV_TIMESTAMP && value.len() == 8).map(|(_, v)| {
        let mut time = [0u8; 8];
        time.copy_from_slice(v);
        time
    });
    let timed = timestamp.is_some();
    let target_info = if server.target_info.is_empty() {
        Vec::new()
    } else {
        client_target_info(&server.target_info, timed, account.channel_binding, account.target_name)
    };
    let hash = ntlm_v2_hash(username, domain, account.password);
    let (response, session_key) = ntlm_v2_response(&hash, &server.challenge, &target_info, timestamp.unwrap_or_else(now), client_challenge);
    let lm = if timed { vec![0u8; 24] } else { [hmac_md5(&hash, &[&server.challenge, &client_challenge]).as_slice(), &client_challenge].concat() };
    let fields = [lm, response, utf16(domain), utf16(username), utf16(account.workstation), Vec::new()];
    // 64 bytes of fields and flags, 8 of version (none), 16 of integrity code, then what the fields point at.
    let mut header = vec![0u8; 88];
    header[..8].copy_from_slice(SIGNATURE);
    header[8..12].copy_from_slice(&3u32.to_le_bytes());
    let mut offset = header.len() as u32;
    for (i, field) in fields.iter().enumerate() {
        let length = field.len() as u16;
        header[12 + i * 8..14 + i * 8].copy_from_slice(&length.to_le_bytes());
        header[14 + i * 8..16 + i * 8].copy_from_slice(&length.to_le_bytes());
        header[16 + i * 8..20 + i * 8].copy_from_slice(&offset.to_le_bytes());
        offset += field.len() as u32;
    }
    let ours = NEGOTIATE_UNICODE | REQUEST_TARGET | NEGOTIATE_NTLM | NEGOTIATE_ALWAYS_SIGN | NEGOTIATE_EXTENDED_SESSIONSECURITY
        | NEGOTIATE_TARGET_INFO | NEGOTIATE_128 | NEGOTIATE_56;
    header[60..64].copy_from_slice(&(server.flags & ours & !NEGOTIATE_VERSION).to_le_bytes());
    let mut message = header;
    for field in &fields {
        message.extend_from_slice(field);
    }
    if timed {
        let mic = hmac_md5(&session_key, &[negotiate, challenge, &message]);
        message[72..88].copy_from_slice(&mic);
    }
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String { b.iter().map(|b| format!("{b:02x}")).collect() }
    fn unhex(s: &str) -> Vec<u8> { (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect() }
    fn utf16_text(b: &[u8]) -> String { String::from_utf16(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap() }

    #[test]
    fn md4_matches_the_rfc() {
        for (text, digest) in [
            ("", "31d6cfe0d16ae931b73c59d7e0c089c0"),
            ("abc", "a448017aaf21d8525fc10ae87aa6729d"),
            ("message digest", "d9130a8164549fe818874806e1c7014b"),
            ("12345678901234567890123456789012345678901234567890123456789012345678901234567890", "e33b4ddc9c38f2199c3e7b164fcc0536"),
        ] {
            assert_eq!(hex(&md4(text.as_bytes())), digest, "{text:?}");
        }
    }

    // [MS-NLMP] 4.2.1 and 4.2.4: User, Domain, Password; the server's challenge and target information; a client
    // challenge of aa…; time zero.
    const SERVER_CHALLENGE: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
    fn target_info() -> Vec<u8> { unhex("02000c0044006f006d00610069006e0001000c0053006500720076006500720000000000") }

    #[test]
    fn ntlmv2_matches_the_protocol_document() {
        assert_eq!(hex(&md4(&utf16("Password"))), "a4f49c406510bdcab6824ee7c30fd852");
        let hash = ntlm_v2_hash("User", "Domain", "Password");
        assert_eq!(hex(&hash), "0c868a403bfd7a93a3001ef22ef02e3f");
        let (response, session_key) = ntlm_v2_response(&hash, &SERVER_CHALLENGE, &target_info(), [0; 8], [0xaa; 8]);
        assert_eq!(hex(&response[..16]), "68cd0ab851e51c96aabc927bebef6a1c");
        assert_eq!(hex(&session_key), "8de40ccadbc14a82f15cb0ad0de95ca3");
    }

    /// A challenge message with these flags and this target information.
    fn challenge_message(flags: u32, info: &[u8]) -> Vec<u8> {
        let mut message = vec![0u8; 56];
        message[..8].copy_from_slice(SIGNATURE);
        message[8..12].copy_from_slice(&2u32.to_le_bytes());
        message[16..20].copy_from_slice(&56u32.to_le_bytes());
        message[20..24].copy_from_slice(&flags.to_le_bytes());
        message[24..32].copy_from_slice(&SERVER_CHALLENGE);
        message[40..42].copy_from_slice(&(info.len() as u16).to_le_bytes());
        message[42..44].copy_from_slice(&(info.len() as u16).to_le_bytes());
        message[44..48].copy_from_slice(&56u32.to_le_bytes());
        message.extend_from_slice(info);
        message
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool { haystack.windows(needle.len()).any(|w| w == needle) }

    #[test]
    fn the_three_messages_are_verifiable() {
        let negotiate = negotiate_message();
        assert_eq!(&negotiate[..8], SIGNATURE);
        assert_eq!(u32_at(&negotiate, 8), 1);
        // A server that says when it is, as Windows does: a timestamp among the target information's pairs.
        let time = unhex("0090d336b734c301");
        let base = target_info();
        let info = [&base[..base.len() - 4], &unhex("07000800"), &time, &[0; 4]].concat();
        let challenge = challenge_message(0xe28a_8215, &info);
        assert_eq!(parse_challenge(&challenge).unwrap().challenge, SERVER_CHALLENGE);
        let account = Account { username: "DOMAIN\\User", domain: "", password: "Password", workstation: "HERE", channel_binding: None, target_name: "" };
        let message = authenticate_message(&negotiate, &challenge, &account).unwrap();
        assert_eq!(u32_at(&message, 8), 3);
        let field = |i: usize| { let at = u32_at(&message, 16 + i * 8) as usize; &message[at..at + u16_at(&message, 12 + i * 8) as usize] };
        assert_eq!(field(0), &[0u8; 24][..]); // no LM response next to a timestamp
        assert_eq!(utf16_text(field(2)), "DOMAIN");
        assert_eq!(utf16_text(field(3)), "User");
        assert_eq!(utf16_text(field(4)), "HERE");
        // The NT response: a proof over the server's challenge and the blob, by the account's hash.
        let nt = field(1);
        let hash = ntlm_v2_hash("User", "DOMAIN", "Password");
        let proof = hmac_md5(&hash, &[&SERVER_CHALLENGE, &nt[16..]]);
        assert_eq!(&nt[..16], &proof);
        assert_eq!(&nt[24..32], &time[..]); // the server's time, as given
        assert!(contains(nt, &unhex("0600040002000000"))); // "an integrity code follows"
        // No TLS connection to bind to: zeros for the channel binding, and an empty target name.
        assert!(contains(nt, &[unhex("0a001000"), vec![0; 16], unhex("09000000"), vec![0; 4]].concat()));
        let binding = b"tls-server-end-point:abc";
        let bound = Account { username: "User", domain: "", password: "Password", workstation: "", channel_binding: Some(binding), target_name: "HTTP/gw" };
        let bound = authenticate_message(&negotiate, &challenge, &bound).unwrap();
        assert!(contains(&bound, &[unhex("0a001000"), channel_binding_hash(binding).to_vec(), unhex("09000e00"), utf16("HTTP/gw"), vec![0; 4]].concat()));
        // The integrity code: over all three messages, with its own place zeroed, by the session key.
        let mut zeroed = message.clone();
        zeroed[72..88].fill(0);
        let key = hmac_md5(&hash, &[&proof]);
        assert_eq!(&message[72..88], &hmac_md5(&key, &[&negotiate, &challenge, &zeroed]));
        // A wrong password gives another proof.
        let other = Account { username: "User", domain: "DOMAIN", password: "password", workstation: "", channel_binding: None, target_name: "" };
        let other = authenticate_message(&negotiate, &challenge, &other).unwrap();
        let at = u32_at(&other, 24) as usize;
        assert_ne!(&other[at..at + 16], &proof);
    }

    #[test]
    fn a_challenge_that_isnt_one_is_refused() {
        assert!(parse_challenge(b"nonsense").unwrap_err().contains("not an NTLM challenge"));
        let account = Account { username: "u", domain: "", password: "p", workstation: "", channel_binding: None, target_name: "" };
        assert!(authenticate_message(&negotiate_message(), &negotiate_message(), &account).unwrap_err().contains("not an NTLM challenge"));
        // Lengths that point past the message.
        let mut cut = challenge_message(NEGOTIATE_TARGET_INFO, &target_info());
        cut.truncate(60);
        assert!(parse_challenge(&cut).is_err());
    }
}
