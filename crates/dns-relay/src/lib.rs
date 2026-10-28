#![doc = include_str!("../README.md")]
#![deny(missing_docs)]
#![cfg_attr(
    test,
    allow(
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::unwrap_used
    )
)]

#[cfg(target_family = "wasm")]
compile_error!("rings-dns-relay is a native-only crate: it binds DNS and TCP sockets");

mod address;
mod announcement;
mod authority;
mod capability;
mod config;
mod ingress;
mod memory;
mod owner;
mod proxy;
mod recall;
mod registry;
mod relay;
mod server;
mod socket;
mod state;
mod zone;

pub use address::AddressBook;
pub use address::AddressCollision;
pub use address::AddressKey;
pub use address::AddressPlan;
pub use address::BookChange;
pub use address::ADDRESS_KEY_LEN;
pub use announcement::Announcement;
pub use announcement::AnnouncementError;
pub use announcement::ProxyProtocol;
pub use announcement::SubName;
pub use announcement::TxtValue;
pub use announcement::MAX_LABEL_LEN;
pub use announcement::MAX_SERVED_PORTS;
pub use announcement::MAX_SUB_NAME_LABELS;
pub use announcement::MAX_TXT_NAMES;
pub use announcement::MAX_TXT_VALUES;
pub use announcement::MAX_TXT_VALUE_LEN;
pub use announcement::SMTP_PORT;
pub use authority::answer;
pub use authority::Announced;
pub use authority::Answer;
pub use authority::AnswerTtl;
pub use authority::View;
pub use capability::Connection;
pub use capability::Directory;
pub use capability::Forward;
pub use config::DnsRelayConfig;
pub use config::ZoneConfig;
pub use ingress::Admission;
pub use ingress::AdmissionLimits;
pub use ingress::Ingress;
pub use ingress::IngressError;
pub use memory::AnnouncementMemory;
pub use memory::MemoryChange;
pub use memory::MemoryWindows;
pub use memory::Observation;
pub use memory::Recalled;
pub use owner::Owner;
pub use proxy::proxy_v2_header;
pub use proxy::ProxyHeaderError;
pub use recall::Recall;
pub use recall::RecallError;
pub use registry::AddressError;
pub use registry::AddressRegistry;
pub use relay::DnsRelay;
pub use relay::DnsRelayError;
pub use server::DnsAuthority;
pub use state::StateError;
pub use zone::Placement;
pub use zone::Zone;
pub use zone::ZoneError;
pub use zone::Zones;
