//! Small, capability-limited application messages carried by the authenticated
//! remote transport.  This is intentionally unrelated to JWM's local IPC.

use super::RemoteResult;
use super::protocol::MessageKind;
use super::x11_input::InputEvent;
use std::io;

const APPLICATION_MAGIC: &[u8; 8] = b"JWMREM01";
const APPLICATION_VERSION: u16 = 1;
const SERVER_HELLO_LEN: usize = 11;
const CLIENT_HELLO_LEN: usize = SERVER_HELLO_LEN + 32;
const FLAG_POINTER: u8 = 1 << 0;
const FLAG_KEYBOARD: u8 = 1 << 1;
const KNOWN_FLAGS: u8 = FLAG_POINTER | FLAG_KEYBOARD;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub request_input: bool,
    pub keymap_fingerprint: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerHello {
    pub pointer_enabled: bool,
    pub keyboard_enabled: bool,
}

impl ClientHello {
    #[must_use]
    pub fn encode(self) -> [u8; CLIENT_HELLO_LEN] {
        let mut payload = [0_u8; CLIENT_HELLO_LEN];
        let flags = if self.request_input { KNOWN_FLAGS } else { 0 };
        payload[..SERVER_HELLO_LEN].copy_from_slice(&encode_hello(flags));
        payload[SERVER_HELLO_LEN..].copy_from_slice(&self.keymap_fingerprint);
        payload
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        if payload.len() != CLIENT_HELLO_LEN {
            return Err(invalid_data("client hello has an invalid length").into());
        }
        let request_input = decode_hello(&payload[..SERVER_HELLO_LEN])? != 0;
        let keymap_fingerprint = payload[SERVER_HELLO_LEN..].try_into().unwrap();
        Ok(Self {
            request_input,
            keymap_fingerprint,
        })
    }
}

impl ServerHello {
    #[must_use]
    pub fn encode(self) -> [u8; SERVER_HELLO_LEN] {
        let mut flags = 0;
        if self.pointer_enabled {
            flags |= FLAG_POINTER;
        }
        if self.keyboard_enabled {
            flags |= FLAG_KEYBOARD;
        }
        encode_hello(flags)
    }

    pub fn decode(payload: &[u8]) -> RemoteResult<Self> {
        let flags = decode_hello(payload)?;
        Ok(Self {
            pointer_enabled: flags & FLAG_POINTER != 0,
            keyboard_enabled: flags & FLAG_KEYBOARD != 0,
        })
    }
}

fn encode_hello(flags: u8) -> [u8; SERVER_HELLO_LEN] {
    let mut payload = [0_u8; SERVER_HELLO_LEN];
    payload[..8].copy_from_slice(APPLICATION_MAGIC);
    payload[8..10].copy_from_slice(&APPLICATION_VERSION.to_be_bytes());
    payload[10] = flags;
    payload
}

fn decode_hello(payload: &[u8]) -> RemoteResult<u8> {
    if payload.len() != SERVER_HELLO_LEN || &payload[..8] != APPLICATION_MAGIC {
        return Err(invalid_data("peer is not using the JWM remote protocol").into());
    }
    let version = u16::from_be_bytes(payload[8..10].try_into().unwrap());
    if version != APPLICATION_VERSION {
        return Err(invalid_data(format!(
            "unsupported JWM remote application version {version}; expected {APPLICATION_VERSION}"
        ))
        .into());
    }
    if payload[10] & !KNOWN_FLAGS != 0 {
        return Err(invalid_data("peer advertised unknown remote capabilities").into());
    }
    Ok(payload[10])
}

#[must_use]
pub fn encode_input(event: InputEvent) -> (MessageKind, Vec<u8>) {
    match event {
        InputEvent::Pointer { x, y } => {
            let mut payload = Vec::with_capacity(4);
            payload.extend_from_slice(&x.to_be_bytes());
            payload.extend_from_slice(&y.to_be_bytes());
            (MessageKind::Pointer, payload)
        }
        InputEvent::Key { keycode, pressed } => {
            (MessageKind::Key, vec![keycode, u8::from(pressed)])
        }
        InputEvent::Button { button, pressed } => {
            (MessageKind::Button, vec![button, u8::from(pressed)])
        }
        InputEvent::ReleaseAll => (MessageKind::ReleaseAll, Vec::new()),
    }
}

pub fn decode_input(kind: MessageKind, payload: &[u8]) -> RemoteResult<InputEvent> {
    match (kind, payload) {
        (MessageKind::Pointer, [x0, x1, y0, y1]) => Ok(InputEvent::Pointer {
            x: u16::from_be_bytes([*x0, *x1]),
            y: u16::from_be_bytes([*y0, *y1]),
        }),
        (MessageKind::Key, [keycode, pressed @ (0 | 1)]) => Ok(InputEvent::Key {
            keycode: *keycode,
            pressed: *pressed != 0,
        }),
        (MessageKind::Button, [button, pressed @ (0 | 1)]) if *button != 0 => {
            Ok(InputEvent::Button {
                button: *button,
                pressed: *pressed != 0,
            })
        }
        (MessageKind::ReleaseAll, []) => Ok(InputEvent::ReleaseAll),
        _ => Err(invalid_data("malformed remote input message").into()),
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_versioned_and_rejects_unknown_flags() {
        let payload = ClientHello {
            request_input: true,
            keymap_fingerprint: [7; 32],
        }
        .encode();
        let decoded = ClientHello::decode(&payload).unwrap();
        assert!(decoded.request_input);
        assert_eq!(decoded.keymap_fingerprint, [7; 32]);

        let mut future = payload;
        future[10] |= 0x80;
        assert!(ClientHello::decode(&future).is_err());
    }

    #[test]
    fn input_messages_round_trip_and_reject_invalid_edges() {
        let events = [
            InputEvent::Pointer { x: 321, y: 654 },
            InputEvent::Key {
                keycode: 38,
                pressed: true,
            },
            InputEvent::Button {
                button: 1,
                pressed: false,
            },
            InputEvent::ReleaseAll,
        ];
        for event in events {
            let (kind, payload) = encode_input(event);
            assert_eq!(decode_input(kind, &payload).unwrap(), event);
        }
        assert!(decode_input(MessageKind::Key, &[38, 2]).is_err());
        assert!(decode_input(MessageKind::Button, &[0, 1]).is_err());
    }

    #[test]
    fn server_can_negotiate_pointer_without_keyboard() {
        let payload = ServerHello {
            pointer_enabled: true,
            keyboard_enabled: false,
        }
        .encode();
        let decoded = ServerHello::decode(&payload).unwrap();
        assert!(decoded.pointer_enabled);
        assert!(!decoded.keyboard_enabled);
    }
}
