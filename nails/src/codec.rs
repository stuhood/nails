use std::io;
use std::str;
use std::path::PathBuf;

use bytes::{BigEndian, ByteOrder, BytesMut, Bytes, BufMut};
use tokio_io::codec::{Encoder, Decoder};

#[derive(Debug)]
pub enum InputChunk {
    Argument(String),
    Environment { key: String, val: String },
    WorkingDir(PathBuf),
    Command(String),
    Heartbeat,
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug)]
pub enum OutputChunk {
    StartReadingStdin,
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

const HEADER_SIZE: usize = 5;

#[derive(Debug)]
pub struct Codec;

impl Decoder for Codec {
    type Item = InputChunk;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // If we have at least a chunk header, decode it to determine how much more we need.
        if buf.len() < HEADER_SIZE {
            return Ok(None);
        }
        let length = BigEndian::read_u32(&buf[0..HEADER_SIZE - 1]);

        // If we have the remainder of the chunk, decode and emit it.
        if buf.len() < HEADER_SIZE + length as usize {
            return Ok(None);
        }

        // Decode the chunk.
        let header = buf.split_to(HEADER_SIZE);
        let mut chunk = buf.split_to(length as usize);
        match header[HEADER_SIZE - 1] {
            b'A' => msg(InputChunk::Argument(to_string(&chunk)?)),
            b'E' => {
                let equals_position = chunk.iter().position(|b| *b == b'=').ok_or_else(|| {
                    err("Environment chunk does not contain `=` separator.")
                })?;
                let key = to_string(&chunk.split_to(equals_position))?;
                let val = to_string(&chunk.split_off(1))?;
                msg(InputChunk::Environment { key, val })
            }
            b'D' => msg(InputChunk::WorkingDir(PathBuf::from(to_string(&chunk)?))),
            b'C' => msg(InputChunk::Command(to_string(&chunk)?)),
            b'H' => msg(InputChunk::Heartbeat),
            b'0' => msg(InputChunk::Stdin(chunk.freeze())),
            b'.' => msg(InputChunk::StdinEOF),
            b => Err(err(&format!(
                "Unrecognized chunk type: {} with len {}",
                b as char,
                length
            ))),
        }
    }
}

impl Encoder for Codec {
    type Item = OutputChunk;
    type Error = io::Error;

    fn encode(&mut self, msg: Self::Item, buf: &mut BytesMut) -> io::Result<()> {
        // Reserve enough space for the header, and then split into header and chunk.
        buf.reserve(HEADER_SIZE);
        buf.put_slice(&[0u8; HEADER_SIZE]);

        // Write chunk data.
        let msg_type = match msg {
            OutputChunk::StartReadingStdin => b'S',
            OutputChunk::Stdout(bytes) => {
                buf.extend_from_slice(&bytes);
                b'1'
            }
            OutputChunk::Stderr(bytes) => {
                buf.extend_from_slice(&bytes);
                b'2'
            }
            OutputChunk::Exit(code) => {
                buf.extend_from_slice(&format!("{}", code).into_bytes());
                b'X'
            }
        };

        // Then write the msg type and body length into the header.
        let buf_len = buf.len() - HEADER_SIZE;
        BigEndian::write_u32(&mut buf[0..HEADER_SIZE - 1], buf_len as u32);
        buf[HEADER_SIZE - 1] = msg_type;

        Ok(())
    }
}

fn msg<T>(message: T) -> Result<Option<T>, io::Error> {
    // We're using the tokio `streaming` pattern, but without bodies: all messages are
    // chunked to be small enough to fully decode.
    Ok(Some(message))
}

pub fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

fn to_string(bytes: &BytesMut) -> Result<String, io::Error> {
    str::from_utf8(bytes).map(|s| s.to_string()).map_err(|e| {
        io::Error::new(io::ErrorKind::Other, e)
    })
}
