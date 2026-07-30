//! Protocolo e transporte do PCShare.
//!
//! Este crate não conhece Windows, captura nem codec. É a costura da decisão
//! D4 em `docs/ARQUITETURA.md`: o Sprint 0 fala TCP, a Fase 2 fala WebRTC, e
//! a troca não pode exigir reescrever a pipeline de vídeo.

pub mod proto;
pub mod secure;
pub mod tcp;
pub mod transport;

pub use proto::{InputEvent, Message, MouseButton, ProtoError};
pub use secure::{Identity, SecureTransport, fingerprint_of};
pub use tcp::TcpTransport;
pub use transport::{Channel, Transport};
