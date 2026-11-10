//! Fixed Noise handshake and application-key envelope. No exported Noise keys.
//! Zeroizing protects our owned buffers, not Snow's internal allocations or
//! compiler-created copies; Snow 0.10's default resolver does not wipe every
//! ephemeral secret on drop. Forward secrecy assumes session secrets erased
//! or otherwise unavailable when the long-term PSK is subsequently compromised.
use super::*;
use rand::TryRng;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const SUITE: &str = "Noise_NNpsk2_25519_ChaChaPoly_SHA256";
const CONTEXT: &[u8] = b"jwm-remote/transport-v3/key-envelope-v1/server-then-client";
const HEADER: usize = TRANSPORT_PREFIX_LEN + NONCE_LEN;
const KEYBOX_LEN: usize = 128;
const FINISHED_LEN: usize = 96;
const NOISE_HANDSHAKE_LEN: usize = 48;

fn domain(label: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    out[..label.len()].copy_from_slice(label);
    out
}
fn fail<T>(_: T) -> ProtocolError {
    ProtocolError::AuthenticationFailed
}
fn header(nonce: [u8; NONCE_LEN]) -> [u8; HEADER] {
    let mut out = [0; HEADER];
    write_transport_prefix(&mut out[..TRANSPORT_PREFIX_LEN]);
    out[TRANSPORT_PREFIX_LEN..].copy_from_slice(&nonce);
    out
}
fn read_header(stream: &mut impl Read) -> Result<[u8; HEADER], ProtocolError> {
    let mut out = [0; HEADER];
    stream.read_exact(&mut out[..TRANSPORT_PREFIX_LEN])?;
    check_transport_prefix(&out[..TRANSPORT_PREFIX_LEN])?;
    stream.read_exact(&mut out[TRANSPORT_PREFIX_LEN..])?;
    Ok(out)
}
fn prologue(server: &[u8; HEADER], client: &[u8; HEADER]) -> Vec<u8> {
    [CONTEXT, SUITE.as_bytes(), server, client].concat()
}
fn noise(psk: &[u8], context: &[u8], client: bool) -> Result<snow::HandshakeState, ProtocolError> {
    let psk: &[u8; PSK_LEN] = psk.try_into().map_err(fail)?;
    let builder = snow::Builder::new(SUITE.parse().map_err(fail)?)
        .prologue(context)
        .map_err(fail)?
        .psk(2, psk)
        .map_err(fail)?;
    if client {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(fail)
}
fn write_flight(stream: &mut impl Write, bytes: &[u8]) -> Result<(), ProtocolError> {
    let len = u16::try_from(bytes.len()).map_err(fail)?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}
fn read_flight<const N: usize>(stream: &mut impl Read) -> Result<[u8; N], ProtocolError> {
    let mut length = [0; 2];
    stream.read_exact(&mut length)?;
    if usize::from(u16::from_be_bytes(length)) != N {
        return Err(fail(()));
    }
    let mut bytes = [0; N];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}
fn write_handshake(
    stream: &mut impl Write,
    state: &mut snow::HandshakeState,
) -> Result<(), ProtocolError> {
    let mut bytes = [0; NOISE_HANDSHAKE_LEN];
    if state.write_message(&[], &mut bytes).map_err(fail)? != bytes.len() {
        return Err(fail(()));
    }
    write_flight(stream, &bytes)
}
fn read_handshake(
    stream: &mut impl Read,
    state: &mut snow::HandshakeState,
) -> Result<(), ProtocolError> {
    let bytes = read_flight::<NOISE_HANDSHAKE_LEN>(stream)?;
    if state.read_message(&bytes, &mut []).map_err(fail)? != 0 {
        return Err(fail(()));
    }
    Ok(())
}
fn seal(
    stream: &mut impl Write,
    state: &mut snow::TransportState,
    plaintext: &[u8],
) -> Result<(), ProtocolError> {
    let mut bytes = [0; KEYBOX_LEN + TAG_LEN];
    let len = state.write_message(plaintext, &mut bytes).map_err(fail)?;
    if len != plaintext.len() + TAG_LEN {
        return Err(fail(()));
    }
    write_flight(stream, &bytes[..len])
}
fn open<const C: usize, const P: usize>(
    stream: &mut impl Read,
    state: &mut snow::TransportState,
) -> Result<Zeroizing<[u8; P]>, ProtocolError> {
    let bytes = read_flight::<C>(stream)?;
    let mut plain = Zeroizing::new([0; P]);
    if state.read_message(&bytes, &mut plain[..]).map_err(fail)? != P {
        return Err(fail(()));
    }
    Ok(plain)
}
fn finished(label: &[u8], hash: &[u8; 32], digest: &[u8; 32]) -> Zeroizing<[u8; FINISHED_LEN]> {
    let mut out = Zeroizing::new([0; FINISHED_LEN]);
    out[..32].copy_from_slice(&domain(label));
    out[32..64].copy_from_slice(hash);
    out[64..].copy_from_slice(digest);
    out
}
fn verify_finished(
    stream: &mut impl Read,
    state: &mut snow::TransportState,
    expected: &[u8; FINISHED_LEN],
) -> Result<(), ProtocolError> {
    let received = open::<112, FINISHED_LEN>(stream, state)?;
    if !constant_time_eq(&received[..], expected) {
        return Err(fail(()));
    }
    Ok(())
}
fn extract(boxed: &[u8; KEYBOX_LEN]) -> SessionKeys {
    let mut keys = SessionKeys {
        c2s: [0; 32],
        s2c: [0; 32],
    };
    keys.c2s.copy_from_slice(&boxed[64..96]);
    keys.s2c.copy_from_slice(&boxed[96..128]);
    keys
}

pub(super) fn client<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    validate_psk(psk)?;
    let server = read_header(stream)?;
    let client = header(nonce);
    stream.write_all(&client)?;
    stream.flush()?;
    let context = prologue(&server, &client);
    let mut hs = noise(psk, &context, true)?;
    write_handshake(stream, &mut hs)?;
    read_handshake(stream, &mut hs)?;
    if !hs.is_handshake_finished() {
        return Err(fail(()));
    }
    let hash: [u8; 32] = hs.get_handshake_hash().try_into().map_err(fail)?;
    let mut state = hs.into_transport_mode().map_err(fail)?;
    let boxed = open::<144, KEYBOX_LEN>(stream, &mut state)?;
    if boxed[..32] != domain(b"jwm-v3/server-keybox/c2s-s2c") || boxed[32..64] != hash {
        return Err(fail(()));
    }
    let digest = Zeroizing::new(<[u8; 32]>::from(Sha256::digest(&boxed[..])));
    seal(
        stream,
        &mut state,
        &finished(b"jwm-v3/client-finished", &hash, &digest)[..],
    )?;
    verify_finished(
        stream,
        &mut state,
        &finished(b"jwm-v3/server-finished", &hash, &digest),
    )?;
    Ok(extract(&boxed))
}

pub(super) fn server<S: Read + Write>(
    stream: &mut S,
    psk: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Result<SessionKeys, ProtocolError> {
    validate_psk(psk)?;
    let server = header(nonce);
    stream.write_all(&server)?;
    stream.flush()?;
    let client = read_header(stream)?;
    let context = prologue(&server, &client);
    let mut hs = noise(psk, &context, false)?;
    read_handshake(stream, &mut hs)?;
    write_handshake(stream, &mut hs)?;
    if !hs.is_handshake_finished() {
        return Err(fail(()));
    }
    let hash: [u8; 32] = hs.get_handshake_hash().try_into().map_err(fail)?;
    let mut state = hs.into_transport_mode().map_err(fail)?;
    let mut boxed = Zeroizing::new([0; KEYBOX_LEN]);
    boxed[..32].copy_from_slice(&domain(b"jwm-v3/server-keybox/c2s-s2c"));
    boxed[32..64].copy_from_slice(&hash);
    rand::rngs::SysRng
        .try_fill_bytes(&mut boxed[64..])
        .map_err(fail)?;
    let digest = Zeroizing::new(<[u8; 32]>::from(Sha256::digest(&boxed[..])));
    seal(stream, &mut state, &boxed[..])?;
    verify_finished(
        stream,
        &mut state,
        &finished(b"jwm-v3/client-finished", &hash, &digest),
    )?;
    seal(
        stream,
        &mut state,
        &finished(b"jwm-v3/server-finished", &hash, &digest)[..],
    )?;
    Ok(extract(&boxed))
}
