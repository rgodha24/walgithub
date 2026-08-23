//! Protobuf store helpers. We delegate to `walgit_store::coord` for
//! `get_message`, `get_message_if_changed`, and `cas_update`, and keep a
//! local `put_message` convenience.

pub use walgit_store::coord::{get_message, get_message_if_changed};
