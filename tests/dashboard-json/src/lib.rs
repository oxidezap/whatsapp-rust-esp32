extern crate self as whatsapp_rust;
pub use async_channel;
pub use bytes;
pub use serde;
pub use serde_json;

#[cfg(test)]
#[path = "../../../src/supervisor/message_log.rs"]
mod message_log;

#[cfg(test)]
#[path = "../../../src/transport/write_completion.rs"]
mod write_completion;

#[cfg(test)]
#[path = "../../../src/storage/session_slots.rs"]
mod session_slots;

#[cfg(test)]
#[path = "../../../src/admin/send_admission.rs"]
mod send_admission;

#[cfg(test)]
#[path = "../../../src/http_client/memory_gate.rs"]
pub mod http_client;
