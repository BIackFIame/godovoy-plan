//! Shared wire format for LAN handshakes, encrypted frames and calendar snapshots.
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};

use blake2::{Blake2s256, Digest};
use flate2::{Compression, bufread::ZlibDecoder, write::ZlibEncoder};
use serde::{Serialize, de::DeserializeOwned};
use snow::{Builder, TransportState};

use crate::{
    Fallible,
    model::{Calendar, Id},
    storage::{CALENDAR_MESSAGE_LIMIT, LimitedWriter},
};

pub(super) const MAGIC: [u8; 8] = *b"YRCAL001";
const NOISE: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
pub(super) const FRAME: usize = 60_000;
pub(super) const MESSAGE_LIMIT: usize = CALENDAR_MESSAGE_LIMIT;
pub(super) const SYNC_HELLO_LEN: usize = 50;

#[derive(Clone, Copy, Eq, PartialEq)]
#[repr(u8)]
pub(super) enum Operation {
    LegacySync = 1,
    Join = 2,
    DigestSync = 3,
}

impl TryFrom<u8> for Operation {
    type Error = &'static str;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::LegacySync),
            2 => Ok(Self::Join),
            3 => Ok(Self::DigestSync),
            _ => Err("Неизвестный тип подключения."),
        }
    }
}

pub(super) fn header(kind: Operation, id: Id) -> [u8; 25] {
    let mut result = [0; 25];
    result[..8].copy_from_slice(&MAGIC);
    result[8] = kind as u8;
    result[9..].copy_from_slice(id.as_bytes());
    result
}

pub(super) fn noise<'a>(header: &'a [u8], key: &'a [u8; 32]) -> Fallible<Builder<'a>> {
    Ok(Builder::new(NOISE.parse()?).prologue(header)?.psk(0, key)?)
}

pub(super) fn send_control(
    stream: &mut impl Write,
    transport: &mut TransportState,
    data: &[u8],
) -> Fallible<()> {
    let mut wire = [0; SYNC_HELLO_LEN + 18];
    send_encrypted(stream, transport, data, &mut wire)
}

pub(super) fn read_control<const N: usize>(
    stream: &mut impl Read,
    transport: &mut TransportState,
) -> Fallible<[u8; N]> {
    let mut wire = [0; SYNC_HELLO_LEN + 16];
    let wire = read_frame_into(stream, &mut wire[..N + 16])?;
    let mut plain = [0; N];
    if transport.read_message(wire, &mut plain)? != N {
        return Err("Неверный размер управляющего сообщения.".into());
    }
    Ok(plain)
}

pub(super) fn write_frame(stream: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let length = u16::try_from(bytes.len()).map_err(io::Error::other)?;
    stream.write_all(&length.to_be_bytes())?;
    stream.write_all(bytes)
}

pub(super) fn read_frame(stream: &mut impl Read, max: usize) -> io::Result<Vec<u8>> {
    let length = read_frame_length(stream, max)?;
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame)?;
    Ok(frame)
}

pub(super) fn read_frame_into<'a>(
    stream: &mut impl Read,
    buffer: &'a mut [u8],
) -> io::Result<&'a [u8]> {
    let length = read_frame_length(stream, buffer.len())?;
    stream.read_exact(&mut buffer[..length])?;
    Ok(&buffer[..length])
}

fn read_frame_length(stream: &mut impl Read, max: usize) -> io::Result<usize> {
    let mut length = [0; 2];
    stream.read_exact(&mut length)?;
    let length = usize::from(u16::from_be_bytes(length));
    if length == 0 || length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Неверный размер сетевого сообщения",
        ));
    }
    Ok(length)
}

pub(super) fn send_secure<T: Serialize>(
    stream: &mut impl Write,
    transport: &mut TransportState,
    value: &T,
) -> Fallible<()> {
    let mut json = Vec::new();
    serde_json::to_writer(LimitedWriter::new(&mut json, MESSAGE_LIMIT), value)?;
    send_bytes(stream, transport, &json)
}

pub(super) fn send_encrypted(
    stream: &mut impl Write,
    transport: &mut TransportState,
    data: &[u8],
    wire: &mut [u8],
) -> Fallible<()> {
    let written = transport.write_message(data, &mut wire[2..])?;
    wire[..2].copy_from_slice(&u16::try_from(written)?.to_be_bytes());
    stream.write_all(&wire[..2 + written])?;
    Ok(())
}

pub(super) fn send_bytes(
    stream: &mut impl Write,
    transport: &mut TransportState,
    bytes: &[u8],
) -> Fallible<()> {
    let length = u32::try_from(bytes.len())?.to_be_bytes();
    send_control(stream, transport, &length)?;
    let mut buffer = vec![0; bytes.len().min(FRAME) + 18];
    for plaintext in bytes.chunks(FRAME) {
        send_encrypted(stream, transport, plaintext, &mut buffer)?;
    }
    Ok(())
}

pub(super) fn read_secure<T: DeserializeOwned>(
    stream: &mut impl Read,
    transport: &mut TransportState,
) -> Fallible<T> {
    Ok(serde_json::from_slice(&read_bytes(stream, transport)?)?)
}

pub(super) fn message_length(
    stream: &mut impl Read,
    transport: &mut TransportState,
) -> Fallible<usize> {
    let length = read_control::<4>(stream, transport)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MESSAGE_LIMIT {
        return Err("Размер календаря превышает лимит синхронизации.".into());
    }
    Ok(length)
}

fn read_bytes(stream: &mut impl Read, transport: &mut TransportState) -> Fallible<Vec<u8>> {
    let length = message_length(stream, transport)?;
    let mut message = vec![0; length];
    let mut buffer = vec![0; length.min(FRAME) + 16];
    for destination in message.chunks_mut(FRAME) {
        let chunk = read_frame_into(stream, &mut buffer[..destination.len() + 16])?;
        if transport.read_message(chunk, destination)? != destination.len() {
            return Err("Получено неполное сообщение.".into());
        }
    }
    Ok(message)
}

struct DigestWriter(Blake2s256);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Digest::update(&mut self.0, bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn calendar_digest(calendar: &impl Serialize) -> Fallible<[u8; 32]> {
    calendar_digest_with_limit(calendar, MESSAGE_LIMIT)
}

pub(super) fn calendar_digest_with_limit(
    calendar: &impl Serialize,
    limit: usize,
) -> Fallible<[u8; 32]> {
    let mut digest = DigestWriter(Blake2s256::new());
    let mut writer = BufWriter::new(LimitedWriter::new(&mut digest, limit));
    serde_json::to_writer(&mut writer, calendar)?;
    writer.into_inner().map_err(|e| e.into_error())?;
    Ok(digest.0.finalize().into())
}

pub(super) fn encode_calendar(calendar: &Calendar) -> Fallible<Vec<u8>> {
    let mut compressed = Vec::new();
    let mut output = LimitedWriter::new(&mut compressed, MESSAGE_LIMIT);
    let mut encoder = ZlibEncoder::new(&mut output, Compression::fast());
    let mut writer = BufWriter::new(LimitedWriter::new(&mut encoder, MESSAGE_LIMIT));
    serde_json::to_writer(&mut writer, calendar)?;
    writer.into_inner().map_err(|e| e.into_error())?;
    encoder.finish()?;
    Ok(compressed)
}

pub(super) fn send_calendar(
    stream: &mut impl Write,
    transport: &mut TransportState,
    calendar: &Calendar,
) -> Fallible<()> {
    send_bytes(stream, transport, &encode_calendar(calendar)?)
}

pub(super) struct CalendarFrames<'a, R> {
    stream: &'a mut R,
    transport: &'a mut TransportState,
    encrypted: Vec<u8>,
    decoded: Vec<u8>,
    unread: std::ops::Range<usize>,
    remaining: usize,
}

impl<'a, R> CalendarFrames<'a, R> {
    pub(super) fn new(stream: &'a mut R, transport: &'a mut TransportState, length: usize) -> Self {
        let capacity = length.min(FRAME);
        Self {
            stream,
            transport,
            encrypted: vec![0; capacity + 16],
            decoded: vec![0; capacity],
            unread: 0..0,
            remaining: length,
        }
    }
}

impl<R: Read> BufRead for CalendarFrames<'_, R> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.unread.is_empty() && self.remaining > 0 {
            let size = self.remaining.min(FRAME);
            let encrypted = read_frame_into(self.stream, &mut self.encrypted[..size + 16])?;
            let count = self
                .transport
                .read_message(encrypted, &mut self.decoded[..size])
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Не удалось прочитать кадр календаря.",
                    )
                })?;
            if count != size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Получено неполное сообщение.",
                ));
            }
            self.remaining -= count;
            self.unread = 0..count;
        }
        Ok(&self.decoded[self.unread.clone()])
    }

    fn consume(&mut self, amount: usize) {
        self.unread.start += amount.min(self.unread.len());
    }
}

impl<R: Read> Read for CalendarFrames<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let count = self.fill_buf()?.read(output)?;
        self.consume(count);
        Ok(count)
    }
}

struct DigestReader<R> {
    source: R,
    digest: Blake2s256,
}

impl<R: Read> Read for DigestReader<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let count = self.source.read(bytes)?;
        Digest::update(&mut self.digest, &bytes[..count]);
        Ok(count)
    }
}

pub(super) fn decode_calendar(
    compressed: impl BufRead,
    length: usize,
    expected: [u8; 32],
) -> Fallible<Calendar> {
    decode_calendar_with_limit(compressed, length, expected, MESSAGE_LIMIT)
}

pub(super) fn decode_calendar_with_limit(
    compressed: impl BufRead,
    length: usize,
    expected: [u8; 32],
    max: usize,
) -> Fallible<Calendar> {
    let decoder = ZlibDecoder::new(compressed);
    let reader = DigestReader {
        source: decoder.take(max as u64 + 1),
        digest: Blake2s256::new(),
    };
    let mut buffered = BufReader::new(reader);
    let calendar = serde_json::from_reader(&mut buffered)?;
    let reader = buffered.into_inner();
    if reader.source.limit() == 0 || reader.source.into_inner().total_in() != length as u64 {
        return Err("Неверный размер сжатого календаря.".into());
    }
    let actual: [u8; 32] = reader.digest.finalize().into();
    if actual != expected {
        return Err("Календарь не совпадает с контрольной суммой обмена.".into());
    }
    Ok(calendar)
}

pub(super) fn read_calendar(
    stream: &mut impl Read,
    transport: &mut TransportState,
    expected: [u8; 32],
) -> Fallible<Calendar> {
    let length = message_length(stream, transport)?;
    decode_calendar(
        CalendarFrames::new(stream, transport, length),
        length,
        expected,
    )
}
