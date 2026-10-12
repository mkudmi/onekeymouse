use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::SocketAddr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use uuid::Uuid;
pub const PROTOCOL_VERSION: u16 = 2;
pub const INPUT_PORT: u16 = 47622;
const MAX_FRAME_SIZE: usize = 64 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    PairRequest {
        version: u16,
        device_id: Uuid,
        device_name: String,
        platform: String,
        public_key: Vec<u8>,
        challenge: Vec<u8>,
    },
    PairAccept {
        version: u16,
        device_id: Uuid,
        public_key: Vec<u8>,
        proof: Vec<u8>,
    },
    PairConfirm {
        proof: Vec<u8>,
    },
    Input {
        sequence: u64,
        event: InputEvent,
    },
    KeepAlive {
        sequence: u64,
    },
    ScreenLayout {
        displays: Vec<crate::desktop::ScreenInfo>,
    },
    ReturnControl {
        position: f64,
    },
    Disconnect {
        reason: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    Enter {
        position: f64,
    },
    Pointer {
        dx: f64,
        dy: f64,
    },
    Button {
        button: MouseButton,
        pressed: bool,
    },
    Wheel {
        horizontal: f32,
        vertical: f32,
    },
    Key {
        code: inputs::PhysicalKey,
        pressed: bool,
    },
    ReleaseAll,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}
#[derive(Default)]
pub struct InputState {
    keys: HashSet<inputs::PhysicalKey>,
    buttons: HashSet<MouseButton>,
}
impl InputState {
    pub fn drag_button(&self) -> Option<MouseButton> {
        [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ]
        .into_iter()
        .find(|b| self.buttons.contains(b))
    }
    pub fn repeated_lock_press(&self, event: &InputEvent) -> bool {
        matches!(
            event,
            InputEvent::Key {
                code: inputs::PhysicalKey::Code(inputs::KeyCode::CapsLock),
                pressed: true
            }
        ) && self
            .keys
            .contains(&inputs::PhysicalKey::Code(inputs::KeyCode::CapsLock))
    }
    pub fn track(&mut self, e: &InputEvent) {
        match e {
            InputEvent::Key { code, pressed } => {
                if *pressed {
                    self.keys.insert(*code);
                } else {
                    self.keys.remove(code);
                }
            }
            InputEvent::Button { button, pressed } => {
                if *pressed {
                    self.buttons.insert(*button);
                } else {
                    self.buttons.remove(button);
                }
            }
            InputEvent::ReleaseAll => {
                self.keys.clear();
                self.buttons.clear();
            }
            _ => {}
        }
    }
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut events = self
            .keys
            .drain()
            .map(|code| InputEvent::Key {
                code,
                pressed: false,
            })
            .collect::<Vec<_>>();
        events.extend(self.buttons.drain().map(|button| InputEvent::Button {
            button,
            pressed: false,
        }));
        events
    }
}

pub async fn write_frame(s: &mut TcpStream, f: &Frame) -> std::io::Result<()> {
    let b = serde_json::to_vec(f).map_err(std::io::Error::other)?;
    if b.len() > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut packet = Vec::with_capacity(4 + b.len());
    packet.extend_from_slice(&(b.len() as u32).to_be_bytes());
    packet.extend_from_slice(&b);
    s.write_all(&packet).await
}
pub async fn read_frame(s: &mut TcpStream) -> std::io::Result<Frame> {
    let n = s.read_u32().await? as usize;
    if n == 0 || n > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid frame size",
        ));
    }
    let mut b = vec![0; n];
    s.read_exact(&mut b).await?;
    serde_json::from_slice(&b).map_err(std::io::Error::other)
}
pub fn peer_addr(host: &str, port: u16) -> Result<SocketAddr, String> {
    format!("{host}:{port}")
        .parse()
        .map_err(|_| "Некорректный адрес компьютера".into())
}

use chacha20poly1305::{
    ChaCha20Poly1305, Nonce,
    aead::{Aead, KeyInit},
};
use hkdf::Hkdf;
use rand::{RngCore, rngs::OsRng};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

pub struct SessionCipher {
    cipher: ChaCha20Poly1305,
    recv_cipher: ChaCha20Poly1305,
    send_prefix: [u8; 4],
    recv_prefix: [u8; 4],
    send_counter: u64,
    recv_counter: u64,
}
impl SessionCipher {
    pub fn new(shared: [u8; 32], initiator: bool) -> Result<Self, String> {
        let hk = Hkdf::<Sha256>::new(Some(b"onekeymouse-v1"), &shared);
        let mut keys = [0u8; 72];
        hk.expand(b"duplex-key-nonce-prefix", &mut keys)
            .map_err(|_| "session key derivation failed")?;
        let (send, recv) = if initiator {
            (&keys[0..32], &keys[32..64])
        } else {
            (&keys[32..64], &keys[0..32])
        };
        let mut sk = [0; 32];
        sk.copy_from_slice(send);
        let mut rk = [0; 32];
        rk.copy_from_slice(recv);
        let cipher = ChaCha20Poly1305::new((&sk).into());
        let recv_cipher = ChaCha20Poly1305::new((&rk).into());
        let send_prefix = if initiator {
            keys[64..68].try_into().unwrap()
        } else {
            keys[68..72].try_into().unwrap()
        };
        let recv_prefix = if initiator {
            keys[68..72].try_into().unwrap()
        } else {
            keys[64..68].try_into().unwrap()
        };
        Ok(Self {
            cipher,
            recv_cipher,
            send_prefix,
            recv_prefix,
            send_counter: 0,
            recv_counter: 0,
        })
    }
    pub fn seal(&mut self, plain: &[u8]) -> Result<Vec<u8>, String> {
        self.send_counter = self.send_counter.checked_add(1).ok_or("nonce exhausted")?;
        let nonce = make_nonce(self.send_prefix, self.send_counter);
        let mut out = self.send_counter.to_be_bytes().to_vec();
        out.extend(
            self.cipher
                .encrypt(Nonce::from_slice(&nonce), plain)
                .map_err(|_| "encryption failed")?,
        );
        Ok(out)
    }
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, String> {
        if frame.len() < 8 {
            return Err("encrypted frame too short".into());
        }
        let counter = u64::from_be_bytes(frame[..8].try_into().unwrap());
        if counter != self.recv_counter + 1 {
            return Err("encrypted frame sequence invalid".into());
        }
        let nonce = make_nonce(self.recv_prefix, counter);
        let plain = self
            .recv_cipher
            .decrypt(Nonce::from_slice(&nonce), &frame[8..])
            .map_err(|_| "authentication failed")?;
        self.recv_counter = counter;
        Ok(plain)
    }
}
fn make_nonce(prefix: [u8; 4], counter: u64) -> [u8; 12] {
    let mut n = [0; 12];
    n[..4].copy_from_slice(&prefix);
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}
pub struct EphemeralKey {
    secret: StaticSecret,
    pubkey: PublicKey,
}
impl EphemeralKey {
    pub fn generate() -> Self {
        let secret = StaticSecret::random_from_rng(OsRng);
        let pubkey = PublicKey::from(&secret);
        Self { secret, pubkey }
    }
    pub fn public(&self) -> [u8; 32] {
        self.pubkey.to_bytes()
    }
    pub fn agree(&self, other: [u8; 32]) -> Result<[u8; 32], String> {
        let shared = self
            .secret
            .diffie_hellman(&PublicKey::from(other))
            .to_bytes();
        if shared.iter().all(|b| *b == 0) {
            Err("invalid peer public key".into())
        } else {
            Ok(shared)
        }
    }
}
pub fn random_challenge() -> [u8; 32] {
    let mut x = [0u8; 32];
    OsRng.fill_bytes(&mut x);
    x
}
pub fn session_proof(shared: &[u8; 32], context: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(shared).expect("hmac key");
    mac.update(b"onekeymouse-pair-v1");
    mac.update(context);
    mac.finalize().into_bytes().into()
}

pub async fn write_secure(
    stream: &mut (impl tokio::io::AsyncWrite + Unpin),
    cipher: &mut SessionCipher,
    frame: &Frame,
) -> std::io::Result<()> {
    let body = serde_json::to_vec(frame).map_err(std::io::Error::other)?;
    let encrypted = cipher.seal(&body).map_err(std::io::Error::other)?;
    if encrypted.len() > MAX_FRAME_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds protocol limit",
        ));
    }
    let mut packet = Vec::with_capacity(4 + encrypted.len());
    packet.extend_from_slice(&(encrypted.len() as u32).to_be_bytes());
    packet.extend_from_slice(&encrypted);
    stream.write_all(&packet).await
}
pub async fn read_secure(
    stream: &mut (impl tokio::io::AsyncRead + Unpin),
    cipher: &mut SessionCipher,
) -> std::io::Result<Frame> {
    let size = stream.read_u32().await? as usize;
    if !(8..=MAX_FRAME_SIZE).contains(&size) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid secure frame size",
        ));
    }
    let mut data = vec![0; size];
    stream.read_exact(&mut data).await?;
    let body = cipher.open(&data).map_err(std::io::Error::other)?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn caps_lock_repeat_does_not_toggle_again_but_next_press_can() {
        let down = InputEvent::Key {
            code: inputs::PhysicalKey::Code(inputs::KeyCode::CapsLock),
            pressed: true,
        };
        let up = InputEvent::Key {
            code: inputs::PhysicalKey::Code(inputs::KeyCode::CapsLock),
            pressed: false,
        };
        let mut state = InputState::default();
        assert!(!state.repeated_lock_press(&down));
        state.track(&down);
        assert!(state.repeated_lock_press(&down));
        assert!(!state.repeated_lock_press(&up));
        state.track(&up);
        assert!(!state.repeated_lock_press(&down));
        state.track(&down);
        state.release_all();
        assert!(!state.repeated_lock_press(&down));
    }

    #[tokio::test]
    async fn secure_frame_prefix_and_body_are_one_write() {
        #[derive(Default)]
        struct Recorder {
            bytes: Vec<u8>,
            writes: usize,
        }
        impl tokio::io::AsyncWrite for Recorder {
            fn poll_write(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                self.writes += 1;
                self.bytes.extend_from_slice(buf);
                std::task::Poll::Ready(Ok(buf.len()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let mut output = Recorder::default();
        let mut sender = SessionCipher::new([31; 32], false).unwrap();
        write_secure(&mut output, &mut sender, &Frame::KeepAlive { sequence: 7 })
            .await
            .unwrap();
        assert_eq!(output.writes, 1);
        let length = u32::from_be_bytes(output.bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(length, output.bytes.len() - 4);
        let mut receiver = SessionCipher::new([31; 32], true).unwrap();
        let body = receiver.open(&output.bytes[4..]).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Frame>(&body).unwrap(),
            Frame::KeepAlive { sequence: 7 }
        ));
    }
    #[test]
    fn held_remote_inputs_release_once_on_return_or_disconnect() {
        let mut state = InputState::default();
        state.track(&InputEvent::Key {
            code: inputs::PhysicalKey::Code(inputs::KeyCode::ControlLeft),
            pressed: true,
        });
        state.track(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
        });
        assert_eq!(state.drag_button(), Some(MouseButton::Left));
        let releases = state.release_all();
        assert_eq!(releases.len(), 2);
        assert!(
            releases
                .iter()
                .any(|e| matches!(e, InputEvent::Key { pressed: false, .. }))
        );
        assert!(
            releases
                .iter()
                .any(|e| matches!(e, InputEvent::Button { pressed: false, .. }))
        );
        assert!(state.release_all().is_empty());
        assert_eq!(state.drag_button(), None);
    }
}
