use std::io;
use std::path::PathBuf;
use std::str;

use std::os::unix::ffi::OsStrExt;

use byteorder::{BigEndian, ByteOrder};
use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Chunk {
    Input(InputChunk),
    Output(OutputChunk),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputChunk {
    Argument(String),
    Environment { key: String, val: String },
    WorkingDir(PathBuf),
    Command(String),
    Heartbeat,
    Stdin(Bytes),
    StdinEof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputChunk {
    StartReadingStdin,
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

const HEADER_SIZE: usize = 5;

#[derive(Debug)]
pub struct ClientCodec;

impl Decoder for ClientCodec {
    type Item = OutputChunk;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        decode(buf).and_then(|opt| match opt {
            None => Ok(None),
            Some(Chunk::Output(o)) => Ok(Some(o)),
            Some(Chunk::Input(i)) => Err(err(&format!(
                "Client received a chunk intended for a server: {:?}",
                i
            ))),
        })
    }
}

impl Encoder<InputChunk> for ClientCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: InputChunk, buf: &mut BytesMut) -> io::Result<()> {
        encode(Chunk::Input(msg), buf);
        Ok(())
    }
}

#[derive(Debug)]
pub struct ServerCodec;

impl Decoder for ServerCodec {
    type Item = InputChunk;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        decode(buf).and_then(|opt| match opt {
            None => Ok(None),
            Some(Chunk::Input(i)) => Ok(Some(i)),
            Some(Chunk::Output(o)) => Err(err(&format!(
                "Server received a chunk intended for a client: {:?}",
                o
            ))),
        })
    }
}

impl Encoder<OutputChunk> for ServerCodec {
    type Error = io::Error;

    fn encode(&mut self, msg: OutputChunk, buf: &mut BytesMut) -> io::Result<()> {
        encode(Chunk::Output(msg), buf);
        Ok(())
    }
}

fn decode(buf: &mut BytesMut) -> Result<Option<Chunk>, io::Error> {
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
        b'A' => input_msg(InputChunk::Argument(to_string(&chunk)?)),
        b'E' => {
            let equals_position = chunk
                .iter()
                .position(|b| *b == b'=')
                .ok_or_else(|| err("Environment chunk does not contain `=` separator."))?;
            let key = to_string(&chunk.split_to(equals_position))?;
            let val = to_string(&chunk.split_off(1))?;
            input_msg(InputChunk::Environment { key, val })
        }
        b'D' => input_msg(InputChunk::WorkingDir(PathBuf::from(to_string(&chunk)?))),
        b'C' => input_msg(InputChunk::Command(to_string(&chunk)?)),
        b'H' => input_msg(InputChunk::Heartbeat),
        b'0' => input_msg(InputChunk::Stdin(chunk.freeze())),
        b'.' => input_msg(InputChunk::StdinEof),
        b'S' => output_msg(OutputChunk::StartReadingStdin),
        b'1' => output_msg(OutputChunk::Stdout(chunk.freeze())),
        b'2' => output_msg(OutputChunk::Stderr(chunk.freeze())),
        b'X' => {
            let exit_code = to_string(&chunk)?
                .parse()
                .map_err(|e| err(&format!("{}", e)))?;
            output_msg(OutputChunk::Exit(exit_code))
        }
        b => Err(err(&format!(
            "Unrecognized chunk type: {} with len {}",
            b as char, length
        ))),
    }
}

fn encode(msg: Chunk, buf: &mut BytesMut) {
    let initial_offset = buf.len();

    // Reserve enough space for the header, and then append the chunk.
    buf.extend_from_slice(&[0u8; HEADER_SIZE]);

    // Write chunk data.
    let msg_type = match msg {
        Chunk::Output(o) => match o {
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
        },
        Chunk::Input(i) => match i {
            InputChunk::Argument(s) => {
                buf.extend_from_slice(s.as_bytes());
                b'A'
            }
            InputChunk::Environment { key, val } => {
                buf.extend_from_slice(key.as_bytes());
                buf.extend_from_slice(b"=");
                buf.extend_from_slice(val.as_bytes());
                b'E'
            }
            InputChunk::WorkingDir(path) => {
                buf.extend_from_slice(path.as_os_str().as_bytes());
                b'D'
            }
            InputChunk::Command(s) => {
                buf.extend_from_slice(s.as_bytes());
                b'C'
            }
            InputChunk::Heartbeat => b'H',
            InputChunk::Stdin(bytes) => {
                buf.extend_from_slice(&bytes);
                b'0'
            }
            InputChunk::StdinEof => b'.',
        },
    };

    // Then write the msg type and body length into the header.
    let header_end = initial_offset + HEADER_SIZE;
    let chunk_len = buf.len() - header_end;
    BigEndian::write_u32(&mut buf[initial_offset..(header_end - 1)], chunk_len as u32);
    buf[header_end - 1] = msg_type;
}

fn input_msg(message: InputChunk) -> Result<Option<Chunk>, io::Error> {
    Ok(Some(Chunk::Input(message)))
}

fn output_msg(message: OutputChunk) -> Result<Option<Chunk>, io::Error> {
    Ok(Some(Chunk::Output(message)))
}

pub fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

fn to_string(bytes: &BytesMut) -> Result<String, io::Error> {
    str::from_utf8(bytes)
        .map(|s| s.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}

#[cfg(test)]
mod tests {
    use super::{decode, encode, Chunk, InputChunk};
    use bytes::BytesMut;

    fn rt(chunks: Vec<Chunk>) {
        let mut buf = {
            let mut buf = BytesMut::with_capacity(1024);
            for chunk in chunks.clone() {
                encode(chunk, &mut buf);
            }
            buf.split()
        };

        let mut decoded_chunks = Vec::new();
        loop {
            match decode(&mut buf) {
                Ok(Some(decoded_chunk)) => decoded_chunks.push(decoded_chunk),
                Ok(None) => break,
                Err(e) => panic!("Failed to decode: {}", e),
            }
        }
        assert_eq!(decoded_chunks, chunks);
    }

    #[test]
    fn roundtrip_chunks() {
        rt(vec![
            Chunk::Input(InputChunk::Argument("Hello".to_owned())),
            Chunk::Input(InputChunk::Environment {
                key: "USER".to_owned(),
                val: "example".to_owned(),
            }),
        ]);
    }
}
