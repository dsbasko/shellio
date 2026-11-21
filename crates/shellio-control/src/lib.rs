//! Управление shellio: протокол v1 управляющего сокета. Без зависимостей от AppKit.

#![forbid(unsafe_code)]

mod codec;
mod error;
mod wire;

pub use codec::{LineError, LineReader, REQUEST_LINE_LIMIT, RESPONSE_LINE_LIMIT, encode_line};
pub use error::{ControlError, ControlErrorCode};
pub use wire::{
    Activation, Hello, HelloReply, Launch, LaunchReceipt, OkValue, OwnerState, PROTOCOL,
    RefuseReason, RefusedArg, Rejected, Request, RequestBody, RequestId, Response,
};
