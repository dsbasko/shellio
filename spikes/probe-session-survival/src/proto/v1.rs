//! Замороженный кодек протокола хоста v1: кадры после рукопожатия в том виде, в каком их писал
//! хост задачи 13. Векторы `vectors/v1.txt` сняты этим кодеком до заморозки и держат формат
//! побайтно. Модуль и векторы не правятся: изменение формата — новая версия в `super`.
//!
//! Правило рукопожатия заморожено вместе с кодеком: `Hello.version` — наибольшая версия клиента,
//! хост отвечает `Welcome` с общей наибольшей версией или `Refused` с причиной. Хост v1 поэтому
//! принимает клиента любого следующего поколения, если тот ещё понимает v1.
//!
//! Чего в v1 нет: признака `ack` у `Input` (версия 2). Кодек v1 его не пишет, разбор даёт
//! `ack = true`: хост v1 подтверждает каждый ввод.

use super::{
    Ack, Body, Fields, Hello, Message, ProtoError, Purpose, Status, Welcome, lead_code, role_code,
};
use super::{
    TAG_CLIPBOARD_REPLY, TAG_CLIPBOARD_REQUEST, TAG_DETACH, TAG_DONE, TAG_EVICTED, TAG_EXITED,
    TAG_FOCUS, TAG_GUI_BIND, TAG_HELLO, TAG_INPUT, TAG_INPUT_ACK, TAG_OUTPUT, TAG_PARSED,
    TAG_REFUSED, TAG_RESIZE, TAG_RESTORE, TAG_ROLE, TAG_SNAPSHOT, TAG_SNAPSHOT_REQUEST, TAG_STATUS,
    TAG_STATUS_REQUEST, TAG_STOP, TAG_VISIBLE, TAG_WELCOME,
};
use crate::backend::Snapshot;

pub const VERSION: u16 = 1;

/// Кадр сообщения в формате v1; признак `ack` у `Input` не пишется.
pub fn encode(message: &Message) -> Vec<u8> {
    let mut body = Body(Vec::new());
    let tag = match message {
        Message::Hello(hello) => {
            body.u16(hello.version);
            body.u8(match hello.purpose {
                Purpose::Attach => 0,
                Purpose::Control => 1,
                Purpose::Gui => 2,
                Purpose::Observe => 3,
            });
            body.u8(lead_code(hello.lead));
            body.str(&hello.client);
            body.size(hello.size);
            TAG_HELLO
        }
        Message::Input {
            source, seq, bytes, ..
        } => {
            body.u64(*source);
            body.u64(*seq);
            body.bytes(bytes);
            TAG_INPUT
        }
        Message::Resize(size) => {
            body.size(*size);
            TAG_RESIZE
        }
        Message::StatusRequest => TAG_STATUS_REQUEST,
        Message::Stop => TAG_STOP,
        Message::Detach => TAG_DETACH,
        Message::SnapshotRequest => TAG_SNAPSHOT_REQUEST,
        Message::GuiBind { generation } => {
            body.u32(*generation);
            TAG_GUI_BIND
        }
        Message::Focus(on) => {
            body.bool(*on);
            TAG_FOCUS
        }
        Message::Visible(on) => {
            body.bool(*on);
            TAG_VISIBLE
        }
        Message::ClipboardReply {
            serial,
            generation,
            data,
        } => {
            body.u64(*serial);
            body.u32(*generation);
            body.opt_bytes(data.as_deref());
            TAG_CLIPBOARD_REPLY
        }
        Message::Restore(bytes) => {
            body.bytes(bytes);
            TAG_RESTORE
        }
        Message::Snapshot(snapshot) => {
            body.u64(snapshot.revision);
            body.u16(snapshot.cols);
            body.u16(snapshot.rows);
            body.u16(snapshot.cursor_x);
            body.u16(snapshot.cursor_y);
            body.bool(snapshot.alternate);
            body.str(&snapshot.text);
            TAG_SNAPSHOT
        }
        Message::ClipboardRequest { serial, generation } => {
            body.u64(*serial);
            body.u32(*generation);
            TAG_CLIPBOARD_REQUEST
        }
        Message::Parsed {
            read,
            offset,
            held,
            ground,
        } => {
            body.u64(*read);
            body.u64(*offset);
            body.u32(*held);
            body.bool(*ground);
            TAG_PARSED
        }
        Message::Welcome(welcome) => {
            body.u16(welcome.version);
            body.str(&welcome.host);
            body.str(&welcome.vt);
            body.str(&welcome.session);
            body.u8(role_code(welcome.role));
            body.u32(welcome.generation);
            body.u64(welcome.offset);
            TAG_WELCOME
        }
        Message::Output { offset, bytes } => {
            body.u64(*offset);
            body.bytes(bytes);
            TAG_OUTPUT
        }
        Message::Status(status) => {
            body.key(status.host);
            body.key(status.workload);
            body.i32(status.workload_pgid);
            body.opt_i32(status.workload_sid);
            body.u32(status.clients);
            body.u8(u8::from(status.leader));
            body.u64(status.reads);
            body.u64(status.read_bytes);
            body.u64(status.parsed_bytes);
            body.u64(status.replies);
            body.u64(status.dropped_replies);
            body.clip(status.clipboard);
            body.size(status.size);
            body.u32(status.generation);
            body.u32(status.evictions);
            body.u64(status.input_queued);
            TAG_STATUS
        }
        Message::Role { role, generation } => {
            body.u8(role_code(*role));
            body.u32(*generation);
            TAG_ROLE
        }
        Message::Exited { code, signal } => {
            body.opt_i32(*code);
            body.opt_i32(*signal);
            TAG_EXITED
        }
        Message::Refused(reason) => {
            body.str(reason);
            TAG_REFUSED
        }
        Message::Done => TAG_DONE,
        Message::InputAck { source, seq, ack } => {
            body.u64(*source);
            body.u64(*seq);
            match ack {
                Ack::Accepted => body.u8(0),
                Ack::Duplicate => body.u8(1),
                Ack::Rejected(reason) => {
                    body.u8(2);
                    body.str(reason);
                }
            }
            TAG_INPUT_ACK
        }
        Message::Evicted(reason) => {
            body.str(reason);
            TAG_EVICTED
        }
    };
    body.frame(tag)
}

/// Сообщение по тегу и телу кадра v1; `Input` получает `ack = true`.
pub fn decode_body(tag: u8, body: &[u8]) -> Result<Message, ProtoError> {
    let mut fields = Fields { rest: body };
    let message = match tag {
        TAG_HELLO => Message::Hello(Hello {
            version: fields.u16("hello.version")?,
            purpose: match fields.u8("hello.purpose")? {
                0 => Purpose::Attach,
                1 => Purpose::Control,
                2 => Purpose::Gui,
                3 => Purpose::Observe,
                _ => return Err(ProtoError::Malformed("hello.purpose")),
            },
            lead: fields.lead()?,
            client: fields.string("hello.client")?,
            size: fields.size("hello.size")?,
        }),
        TAG_INPUT => Message::Input {
            source: fields.u64("input.source")?,
            seq: fields.u64("input.seq")?,
            bytes: fields.bytes("input.bytes")?,
            ack: true,
        },
        TAG_RESIZE => Message::Resize(fields.size("resize")?),
        TAG_STATUS_REQUEST => Message::StatusRequest,
        TAG_STOP => Message::Stop,
        TAG_DETACH => Message::Detach,
        TAG_SNAPSHOT_REQUEST => Message::SnapshotRequest,
        TAG_GUI_BIND => Message::GuiBind {
            generation: fields.u32("gui_bind.generation")?,
        },
        TAG_FOCUS => Message::Focus(fields.bool("focus")?),
        TAG_VISIBLE => Message::Visible(fields.bool("visible")?),
        TAG_CLIPBOARD_REPLY => Message::ClipboardReply {
            serial: fields.u64("clipboard_reply.serial")?,
            generation: fields.u32("clipboard_reply.generation")?,
            data: fields.opt_bytes("clipboard_reply.data")?,
        },
        TAG_RESTORE => Message::Restore(fields.bytes("restore")?),
        TAG_SNAPSHOT => Message::Snapshot(Snapshot {
            revision: fields.u64("snapshot.revision")?,
            cols: fields.u16("snapshot.cols")?,
            rows: fields.u16("snapshot.rows")?,
            cursor_x: fields.u16("snapshot.cursor_x")?,
            cursor_y: fields.u16("snapshot.cursor_y")?,
            alternate: fields.bool("snapshot.alternate")?,
            text: fields.string("snapshot.text")?,
        }),
        TAG_CLIPBOARD_REQUEST => Message::ClipboardRequest {
            serial: fields.u64("clipboard_request.serial")?,
            generation: fields.u32("clipboard_request.generation")?,
        },
        TAG_PARSED => Message::Parsed {
            read: fields.u64("parsed.read")?,
            offset: fields.u64("parsed.offset")?,
            held: fields.u32("parsed.held")?,
            ground: fields.bool("parsed.ground")?,
        },
        TAG_WELCOME => Message::Welcome(Welcome {
            version: fields.u16("welcome.version")?,
            host: fields.string("welcome.host")?,
            vt: fields.string("welcome.vt")?,
            session: fields.string("welcome.session")?,
            role: fields.role()?,
            generation: fields.u32("welcome.generation")?,
            offset: fields.u64("welcome.offset")?,
        }),
        TAG_OUTPUT => Message::Output {
            offset: fields.u64("output.offset")?,
            bytes: fields.bytes("output.bytes")?,
        },
        TAG_STATUS => Message::Status(Status {
            host: fields.key("status.host")?,
            workload: fields.key("status.workload")?,
            workload_pgid: fields.i32("status.pgid")?,
            workload_sid: fields.opt_i32("status.sid")?,
            clients: fields.u32("status.clients")?,
            leader: match fields.u8("status.leader")? {
                0 => false,
                1 => true,
                _ => return Err(ProtoError::Malformed("status.leader")),
            },
            reads: fields.u64("status.reads")?,
            read_bytes: fields.u64("status.read_bytes")?,
            parsed_bytes: fields.u64("status.parsed_bytes")?,
            replies: fields.u64("status.replies")?,
            dropped_replies: fields.u64("status.dropped_replies")?,
            clipboard: fields.clip()?,
            size: fields.size("status.size")?,
            generation: fields.u32("status.generation")?,
            evictions: fields.u32("status.evictions")?,
            input_queued: fields.u64("status.input_queued")?,
        }),
        TAG_ROLE => Message::Role {
            role: fields.role()?,
            generation: fields.u32("role.generation")?,
        },
        TAG_EXITED => Message::Exited {
            code: fields.opt_i32("exited.code")?,
            signal: fields.opt_i32("exited.signal")?,
        },
        TAG_REFUSED => Message::Refused(fields.string("refused")?),
        TAG_DONE => Message::Done,
        TAG_INPUT_ACK => Message::InputAck {
            source: fields.u64("input_ack.source")?,
            seq: fields.u64("input_ack.seq")?,
            ack: match fields.u8("input_ack.ack")? {
                0 => Ack::Accepted,
                1 => Ack::Duplicate,
                2 => Ack::Rejected(fields.string("input_ack.reason")?),
                _ => return Err(ProtoError::Malformed("input_ack.ack")),
            },
        },
        TAG_EVICTED => Message::Evicted(fields.string("evicted")?),
        other => return Err(ProtoError::UnknownTag(other)),
    };
    fields.end()?;
    Ok(message)
}
