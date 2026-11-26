use std::io;
use std::os::fd::RawFd;

use crate::linux::{
    read_exact_fd, DeviceInfo, FfEffect, FfEffectU, FfReplay, FfRumbleEffect, FfTrigger,
    InputAbsInfo, InputId, ABS_BITMAP_LEN, EVENT_SIZE, FF_BITMAP_LEN, FF_RUMBLE, KEY_BITMAP_LEN,
    MSC_BITMAP_LEN,
};

// Refuse to queue more than this for a peer that has stopped reading. Input
// keeps flowing while the socket is backed up, but a peer that is gone for
// long enough has to be reconnected rather than growing without bound.
const MAX_QUEUED: usize = 4 << 20;

pub const MAGIC: u32 = 0x594c_5258;
// The magic bytes are "XRLY" in little-endian order.
pub const PROTOCOL_VERSION: u16 = 1;
const MAX_FRAME: usize = 1 << 20;
const MAX_NAME: usize = 256;

const MSG_HELLO: u8 = 1;
const MSG_DEVICE_ADD: u8 = 2;
const MSG_DEVICE_REMOVE: u8 = 3;
const MSG_EVENTS: u8 = 4;
const MSG_FF_UPLOAD: u8 = 5;
const MSG_FF_UPLOAD_RESP: u8 = 6;
const MSG_FF_ERASE: u8 = 7;
const MSG_FF_ERASE_RESP: u8 = 8;
const MSG_FF_PLAY: u8 = 9;

#[derive(Clone, Copy, Debug)]
pub struct FfUploadData {
    pub type_: u16,
    pub direction: u16,
    pub trigger: FfTrigger,
    pub replay: FfReplay,
    pub strong: u16,
    pub weak: u16,
}

impl FfUploadData {
    pub fn from_effect(effect: &FfEffect) -> Option<Self> {
        if effect.type_ != FF_RUMBLE {
            return None;
        }
        // SAFETY: The effect type is FF_RUMBLE. The producer initialized the
        // rumble variant.
        let rumble = unsafe { effect.u.rumble };
        Some(Self {
            type_: effect.type_,
            direction: effect.direction,
            trigger: effect.trigger,
            replay: effect.replay,
            strong: rumble.strong_magnitude,
            weak: rumble.weak_magnitude,
        })
    }

    pub fn to_effect(self) -> FfEffect {
        FfEffect {
            type_: self.type_,
            id: -1,
            direction: self.direction,
            trigger: self.trigger,
            replay: self.replay,
            u: FfEffectU {
                rumble: FfRumbleEffect {
                    strong_magnitude: self.strong,
                    weak_magnitude: self.weak,
                },
            },
        }
    }
}

#[derive(Clone, Debug)]
pub enum Message {
    Hello {
        version: u16,
    },
    DeviceAdd {
        slot: u8,
        info: DeviceInfo,
    },
    DeviceRemove {
        slot: u8,
    },
    Events {
        slot: u8,
        data: Vec<u8>,
    },
    FfUpload {
        slot: u8,
        local_id: u16,
        effect: FfUploadData,
    },
    FfUploadResp {
        slot: u8,
        local_id: u16,
        result: i32,
        host_id: u16,
    },
    FfErase {
        slot: u8,
        local_id: u16,
    },
    FfEraseResp {
        slot: u8,
        local_id: u16,
        result: i32,
    },
    FfPlay {
        slot: u8,
        code: u16,
        value: i32,
    },
}

impl Message {
    pub fn encode_frame(&self) -> Vec<u8> {
        let mut body = Vec::new();
        match self {
            Message::Hello { version } => {
                body.push(MSG_HELLO);
                put_u32(&mut body, MAGIC);
                put_u16(&mut body, *version);
            }
            Message::DeviceAdd { slot, info } => {
                body.push(MSG_DEVICE_ADD);
                body.push(*slot);
                encode_device_info(&mut body, info);
            }
            Message::DeviceRemove { slot } => {
                body.push(MSG_DEVICE_REMOVE);
                body.push(*slot);
            }
            Message::Events { slot, data } => {
                body.push(MSG_EVENTS);
                body.push(*slot);
                body.extend_from_slice(data);
            }
            Message::FfUpload {
                slot,
                local_id,
                effect,
            } => {
                body.push(MSG_FF_UPLOAD);
                body.push(*slot);
                put_u16(&mut body, *local_id);
                put_u16(&mut body, effect.type_);
                put_u16(&mut body, effect.direction);
                put_u16(&mut body, effect.trigger.button);
                put_u16(&mut body, effect.trigger.interval);
                put_u16(&mut body, effect.replay.length);
                put_u16(&mut body, effect.replay.delay);
                put_u16(&mut body, effect.strong);
                put_u16(&mut body, effect.weak);
            }
            Message::FfUploadResp {
                slot,
                local_id,
                result,
                host_id,
            } => {
                body.push(MSG_FF_UPLOAD_RESP);
                body.push(*slot);
                put_u16(&mut body, *local_id);
                put_i32(&mut body, *result);
                put_u16(&mut body, *host_id);
            }
            Message::FfErase { slot, local_id } => {
                body.push(MSG_FF_ERASE);
                body.push(*slot);
                put_u16(&mut body, *local_id);
            }
            Message::FfEraseResp {
                slot,
                local_id,
                result,
            } => {
                body.push(MSG_FF_ERASE_RESP);
                body.push(*slot);
                put_u16(&mut body, *local_id);
                put_i32(&mut body, *result);
            }
            Message::FfPlay { slot, code, value } => {
                body.push(MSG_FF_PLAY);
                body.push(*slot);
                put_u16(&mut body, *code);
                put_i32(&mut body, *value);
            }
        }

        let mut frame = Vec::with_capacity(body.len() + 4);
        put_u32(&mut frame, body.len() as u32);
        frame.extend_from_slice(&body);
        frame
    }

    pub fn decode(body: &[u8]) -> Result<Self, String> {
        let mut cursor = Cursor::new(body);
        let message_type = cursor.u8()?;
        let message = match message_type {
            MSG_HELLO => {
                if cursor.u32()? != MAGIC {
                    return Err("bad hello magic".into());
                }
                Message::Hello {
                    version: cursor.u16()?,
                }
            }
            MSG_DEVICE_ADD => {
                let slot = cursor.u8()?;
                Message::DeviceAdd {
                    slot,
                    info: decode_device_info(&mut cursor)?,
                }
            }
            MSG_DEVICE_REMOVE => Message::DeviceRemove { slot: cursor.u8()? },
            MSG_EVENTS => {
                let slot = cursor.u8()?;
                let data = cursor.bytes(cursor.remaining())?.to_vec();
                Message::Events { slot, data }
            }
            MSG_FF_UPLOAD => {
                let slot = cursor.u8()?;
                let local_id = cursor.u16()?;
                let effect = FfUploadData {
                    type_: cursor.u16()?,
                    direction: cursor.u16()?,
                    trigger: FfTrigger {
                        button: cursor.u16()?,
                        interval: cursor.u16()?,
                    },
                    replay: FfReplay {
                        length: cursor.u16()?,
                        delay: cursor.u16()?,
                    },
                    strong: cursor.u16()?,
                    weak: cursor.u16()?,
                };
                Message::FfUpload {
                    slot,
                    local_id,
                    effect,
                }
            }
            MSG_FF_UPLOAD_RESP => {
                let slot = cursor.u8()?;
                let local_id = cursor.u16()?;
                let result = cursor.i32()?;
                let host_id = cursor.u16()?;
                Message::FfUploadResp {
                    slot,
                    local_id,
                    result,
                    host_id,
                }
            }
            MSG_FF_ERASE => {
                let slot = cursor.u8()?;
                let local_id = cursor.u16()?;
                Message::FfErase { slot, local_id }
            }
            MSG_FF_ERASE_RESP => {
                let slot = cursor.u8()?;
                let local_id = cursor.u16()?;
                let result = cursor.i32()?;
                Message::FfEraseResp {
                    slot,
                    local_id,
                    result,
                }
            }
            MSG_FF_PLAY => {
                let slot = cursor.u8()?;
                let code = cursor.u16()?;
                let value = cursor.i32()?;
                Message::FfPlay { slot, code, value }
            }
            _ => return Err(format!("unknown message type {message_type}")),
        };

        if cursor.remaining() != 0 {
            return Err("trailing bytes in message".into());
        }
        Ok(message)
    }
}

pub fn read_frame(fd: RawFd) -> io::Result<Message> {
    let mut prefix = [0u8; 4];
    read_exact_fd(fd, &mut prefix)?;
    let len = u32::from_le_bytes(prefix) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad frame length",
        ));
    }
    let mut body = vec![0u8; len];
    read_exact_fd(fd, &mut body)?;
    Message::decode(&body).map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Buffers bytes from a non-blocking socket and yields complete frames.
#[derive(Default)]
pub struct FrameParser {
    buffer: Vec<u8>,
    head: usize,
}

impl FrameParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Read one chunk from a non-blocking socket.
    ///
    /// Returns `false` when the peer closed the connection. Processing one
    /// chunk per poll keeps a backlogged link from monopolising the loop.
    pub fn read_from(&mut self, fd: RawFd) -> io::Result<bool> {
        let mut chunk = [0u8; 65536];
        loop {
            // SAFETY: chunk is writable for its length and fd is an open socket.
            let ret = unsafe {
                libc::recv(
                    fd,
                    chunk.as_mut_ptr() as *mut libc::c_void,
                    chunk.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if ret > 0 {
                self.push(&chunk[..ret as usize]);
                return Ok(true);
            }
            if ret == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(true),
                _ => return Err(error),
            }
        }
    }

    pub fn next(&mut self) -> io::Result<Option<Message>> {
        let available = &self.buffer[self.head..];
        if available.len() < 4 {
            return Ok(None);
        }
        let len =
            u32::from_le_bytes([available[0], available[1], available[2], available[3]]) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame length",
            ));
        }
        if available.len() < 4 + len {
            return Ok(None);
        }
        let message = Message::decode(&available[4..4 + len])
            .map_err(|message| io::Error::new(io::ErrorKind::InvalidData, message))?;
        self.head += 4 + len;
        if self.head == self.buffer.len() {
            self.buffer.clear();
            self.head = 0;
        } else if self.head >= 1 << 16 {
            self.buffer.drain(..self.head);
            self.head = 0;
        }
        Ok(Some(message))
    }
}

/// Queues frames and writes as much as the socket accepts without blocking.
#[derive(Default)]
pub struct FrameWriter {
    buffer: Vec<u8>,
    head: usize,
}

impl FrameWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.head == self.buffer.len()
    }

    pub fn queue(&mut self, message: &Message) -> io::Result<()> {
        let frame = message.encode_frame();
        if self.buffer.len() - self.head + frame.len() > MAX_QUEUED {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "peer is not reading",
            ));
        }
        self.buffer.extend_from_slice(&frame);
        Ok(())
    }

    pub fn flush(&mut self, fd: RawFd) -> io::Result<()> {
        while self.head < self.buffer.len() {
            let data = &self.buffer[self.head..];
            // SAFETY: data covers len readable bytes and fd is an open socket.
            let ret = unsafe {
                libc::send(
                    fd,
                    data.as_ptr() as *const libc::c_void,
                    data.len(),
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
            if ret > 0 {
                self.head += ret as usize;
                continue;
            }
            if ret == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "send returned 0"));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => break,
                _ => return Err(error),
            }
        }
        if self.head == self.buffer.len() {
            self.buffer.clear();
            self.head = 0;
        } else if self.head >= 1 << 16 {
            self.buffer.drain(..self.head);
            self.head = 0;
        }
        Ok(())
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn encode_device_info(out: &mut Vec<u8>, info: &DeviceInfo) {
    let name = info.name.as_bytes();
    put_u16(out, name.len() as u16);
    out.extend_from_slice(name);
    put_u16(out, info.id.bustype);
    put_u16(out, info.id.vendor);
    put_u16(out, info.id.product);
    put_u16(out, info.id.version);
    put_u16(out, info.key.len() as u16);
    out.extend_from_slice(&info.key);
    put_u16(out, info.abs.len() as u16);
    for (code, abs) in &info.abs {
        put_u16(out, *code);
        put_i32(out, abs.minimum);
        put_i32(out, abs.maximum);
        put_i32(out, abs.fuzz);
        put_i32(out, abs.flat);
        put_i32(out, abs.resolution);
    }
    put_u16(out, info.ff.len() as u16);
    out.extend_from_slice(&info.ff);
    put_u16(out, info.msc.len() as u16);
    out.extend_from_slice(&info.msc);
    put_u16(out, info.uniq.len() as u16);
    out.extend_from_slice(info.uniq.as_bytes());
    put_u16(out, info.phys.len() as u16);
    out.extend_from_slice(info.phys.as_bytes());
}

fn decode_device_info(cursor: &mut Cursor<'_>) -> Result<DeviceInfo, String> {
    let name_len = cursor.u16()? as usize;
    if name_len > MAX_NAME {
        return Err("bad device name length".into());
    }
    let name = String::from_utf8(cursor.bytes(name_len)?.to_vec())
        .map_err(|_| "device name is not UTF-8")?;
    let id = InputId {
        bustype: cursor.u16()?,
        vendor: cursor.u16()?,
        product: cursor.u16()?,
        version: cursor.u16()?,
    };

    let key_len = cursor.u16()? as usize;
    if key_len > KEY_BITMAP_LEN {
        return Err("bad key bitmap length".into());
    }
    let key = cursor.bytes(key_len)?.to_vec();

    let abs_count = cursor.u16()? as usize;
    if abs_count > ABS_BITMAP_LEN * 8 {
        return Err("bad abs count".into());
    }
    let mut abs = Vec::with_capacity(abs_count);
    for _ in 0..abs_count {
        let code = cursor.u16()?;
        let absinfo = InputAbsInfo {
            value: 0,
            minimum: cursor.i32()?,
            maximum: cursor.i32()?,
            fuzz: cursor.i32()?,
            flat: cursor.i32()?,
            resolution: cursor.i32()?,
        };
        abs.push((code, absinfo));
    }

    let ff_len = cursor.u16()? as usize;
    if ff_len > FF_BITMAP_LEN {
        return Err("bad ff bitmap length".into());
    }
    let ff = cursor.bytes(ff_len)?.to_vec();

    let msc_len = cursor.u16()? as usize;
    if msc_len > MSC_BITMAP_LEN {
        return Err("bad msc bitmap length".into());
    }
    let msc = cursor.bytes(msc_len)?.to_vec();

    let uniq_len = cursor.u16()? as usize;
    if uniq_len > MAX_NAME {
        return Err("bad uniq length".into());
    }
    let uniq = String::from_utf8(cursor.bytes(uniq_len)?.to_vec())
        .map_err(|_| "device uniq is not UTF-8")?;

    let phys_len = cursor.u16()? as usize;
    if phys_len > MAX_NAME {
        return Err("bad phys length".into());
    }
    let phys = String::from_utf8(cursor.bytes(phys_len)?.to_vec())
        .map_err(|_| "device phys is not UTF-8")?;

    Ok(DeviceInfo {
        name,
        id,
        key,
        abs,
        ff,
        msc,
        uniq,
        phys,
    })
}

struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    fn bytes(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or("message size overflow")?;
        let slice = self.data.get(self.offset..end).ok_or("truncated message")?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(self.u32()? as i32)
    }
}

pub fn events_are_aligned(data: &[u8]) -> bool {
    !data.is_empty() && data.len().is_multiple_of(EVENT_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> DeviceInfo {
        DeviceInfo {
            name: "Microsoft Xbox Controller".into(),
            id: InputId {
                bustype: 0x0006,
                vendor: 0x045e,
                product: 0x0b00,
                version: 0x0509,
            },
            key: {
                let mut bitmap = vec![0u8; KEY_BITMAP_LEN];
                for code in [0x130, 0x131, 0x133, 0x134] {
                    bitmap[code / 8] |= 1 << (code % 8);
                }
                bitmap
            },
            abs: vec![(
                0x00,
                InputAbsInfo {
                    value: 0,
                    minimum: -32768,
                    maximum: 32767,
                    fuzz: 16,
                    flat: 128,
                    resolution: 0,
                },
            )],
            ff: {
                let mut bitmap = vec![0u8; FF_BITMAP_LEN];
                bitmap[FF_RUMBLE as usize / 8] |= 1 << (FF_RUMBLE as usize % 8);
                bitmap
            },
            msc: vec![1],
            uniq: "01:02:03:04:05:06".into(),
            phys: "usb-0000:00:14.0-1/input0".into(),
        }
    }

    fn round_trip(message: &Message) -> Message {
        let frame = message.encode_frame();
        let len = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, frame.len() - 4);
        Message::decode(&frame[4..]).expect("decode")
    }

    #[test]
    fn hello_round_trips() {
        match round_trip(&Message::Hello {
            version: PROTOCOL_VERSION,
        }) {
            Message::Hello { version } => assert_eq!(version, PROTOCOL_VERSION),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn device_add_round_trips() {
        let info = sample_info();
        let decoded = round_trip(&Message::DeviceAdd {
            slot: 3,
            info: info.clone(),
        });
        match decoded {
            Message::DeviceAdd {
                slot,
                info: decoded,
            } => {
                assert_eq!(slot, 3);
                assert_eq!(decoded.name, info.name);
                assert_eq!(decoded.id.vendor, 0x045e);
                assert_eq!(decoded.key, info.key);
                assert_eq!(decoded.abs.len(), 1);
                assert_eq!(decoded.ff, info.ff);
                assert_eq!(decoded.uniq, info.uniq);
                assert_eq!(decoded.phys, info.phys);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn events_round_trip() {
        let data = vec![0u8; EVENT_SIZE * 2];
        match round_trip(&Message::Events {
            slot: 1,
            data: data.clone(),
        }) {
            Message::Events {
                slot,
                data: decoded,
            } => {
                assert_eq!(slot, 1);
                assert_eq!(decoded, data);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ff_round_trips() {
        let effect = FfUploadData {
            type_: FF_RUMBLE,
            direction: 0,
            trigger: FfTrigger {
                button: 0,
                interval: 0,
            },
            replay: FfReplay {
                length: 500,
                delay: 10,
            },
            strong: 0x8000,
            weak: 0x4000,
        };
        match round_trip(&Message::FfUpload {
            slot: 2,
            local_id: 7,
            effect,
        }) {
            Message::FfUpload {
                slot,
                local_id,
                effect: decoded,
            } => {
                assert_eq!(slot, 2);
                assert_eq!(local_id, 7);
                assert_eq!(decoded.strong, 0x8000);
                assert_eq!(decoded.weak, 0x4000);
                assert_eq!(decoded.replay.length, 500);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ff_effect_conversion_keeps_rumble() {
        let data = FfUploadData {
            type_: FF_RUMBLE,
            direction: 0x4000,
            trigger: FfTrigger::default(),
            replay: FfReplay {
                length: 100,
                delay: 0,
            },
            strong: 1,
            weak: 2,
        };
        let effect = data.to_effect();
        assert_eq!(effect.id, -1);
        // SAFETY: to_effect initialized the rumble variant.
        let rumble = unsafe { effect.u.rumble };
        assert_eq!(rumble.strong_magnitude, 1);
        assert_eq!(rumble.weak_magnitude, 2);
    }

    #[test]
    fn rejects_bad_hello_magic() {
        let mut frame = Message::Hello { version: 2 }.encode_frame();
        frame[5] ^= 0xff;
        assert!(Message::decode(&frame[4..]).is_err());
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut frame = Message::DeviceRemove { slot: 1 }.encode_frame();
        frame.push(0);
        let len = (frame.len() - 4) as u32;
        frame[..4].copy_from_slice(&len.to_le_bytes());
        assert!(Message::decode(&frame[4..]).is_err());
    }

    #[test]
    fn event_alignment() {
        assert!(events_are_aligned(&[0u8; EVENT_SIZE]));
        assert!(events_are_aligned(&[0u8; EVENT_SIZE * 3]));
        assert!(!events_are_aligned(&[]));
        assert!(!events_are_aligned(&[0u8; EVENT_SIZE - 1]));
    }

    #[test]
    fn parser_waits_for_a_complete_frame() {
        let frame = Message::DeviceRemove { slot: 3 }.encode_frame();
        let mut parser = FrameParser::new();
        parser.push(&frame[..2]);
        assert!(parser.next().expect("parse").is_none());
        parser.push(&frame[2..5]);
        assert!(parser.next().expect("parse").is_none());
        parser.push(&frame[5..]);
        match parser.next().expect("parse") {
            Some(Message::DeviceRemove { slot }) => assert_eq!(slot, 3),
            other => panic!("unexpected {other:?}"),
        }
        assert!(parser.next().expect("parse").is_none());
    }

    #[test]
    fn parser_returns_every_buffered_frame() {
        let mut bytes = Message::DeviceRemove { slot: 1 }.encode_frame();
        bytes.extend_from_slice(&Message::DeviceRemove { slot: 2 }.encode_frame());
        let mut parser = FrameParser::new();
        parser.push(&bytes);
        for slot in [1, 2] {
            match parser.next().expect("parse") {
                Some(Message::DeviceRemove { slot: decoded }) => assert_eq!(decoded, slot),
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(parser.next().expect("parse").is_none());
    }

    #[test]
    fn parser_rejects_a_bad_frame_length() {
        let mut frame = Message::DeviceRemove { slot: 1 }.encode_frame();
        frame[..4].copy_from_slice(&0u32.to_le_bytes());
        let mut parser = FrameParser::new();
        parser.push(&frame);
        assert!(parser.next().is_err());
    }

    #[test]
    fn writer_flushes_a_complete_frame() {
        let mut fds = [0; 2];
        // SAFETY: fds is writable for two descriptors.
        let ret =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(ret, 0);

        let mut writer = FrameWriter::new();
        writer
            .queue(&Message::DeviceRemove { slot: 9 })
            .expect("queue");
        assert!(!writer.is_empty());
        writer.flush(fds[0]).expect("flush");
        assert!(writer.is_empty());

        let mut parser = FrameParser::new();
        assert!(parser.read_from(fds[1]).expect("read"));
        match parser.next().expect("parse") {
            Some(Message::DeviceRemove { slot }) => assert_eq!(slot, 9),
            other => panic!("unexpected {other:?}"),
        }

        // SAFETY: both descriptors are open and owned by this test.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
