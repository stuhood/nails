use std::io;
use std::str;
use bytes::{BigEndian, ByteOrder, BytesMut, Bytes, BufMut};
use tokio_io::codec::{Encoder, Decoder};
use tokio_proto::streaming::pipeline::Frame;


pub enum InputChunk {
    Argument(String),
    Environment {
      key: String,
      val: String
    },
    WorkingDir(String),
    Command(String),
    Stdin(Bytes),
    StdinEOF,
}

pub enum OutputChunk {
    Pid(usize),
    StartReadingInput,
    Stdout(Bytes),
    Stderr(Bytes),
    Exit,
}

const HEADER_SIZE: usize = 5;

pub struct Codec;

impl Decoder for Codec {
    type Item = Frame<InputChunk, (), io::Error>;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // If we have at least a chunk header, decode it to determine how much more we need.
        if buf.len() < HEADER_SIZE {
            return Ok(None);
        }
        let length = BigEndian::read_u32(&buf[1..HEADER_SIZE]);

        // If we have the remainder of the chunk, decode and emit it.
        let chunk_length = HEADER_SIZE + length as usize;
        if buf.len() < chunk_length {
            return Ok(None)
        }

        // Decode the chunk.
        let header = buf.split_to(HEADER_SIZE);
        let mut chunk = buf.split_to(chunk_length);
        match header[0] {
            b'A' => msg(InputChunk::Argument(to_string(&chunk)?)),
            b'E' => {
              let equals_position =
                chunk
                  .iter()
                  .position(|b| *b == b'=')
                  .ok_or_else(|| err("Environment chunk does not contain `=` separator."))?;
              let key = to_string(&chunk.split_to(equals_position))?;
              let val = to_string(&chunk.split_off(1))?;
              msg(InputChunk::Environment { key, val })
            },
            b'D' => msg(InputChunk::WorkingDir(to_string(&chunk)?)),
            b'C' => msg(InputChunk::Command(to_string(&chunk)?)),
            b'0' => msg(InputChunk::Stdin(chunk.freeze())),
            b'.' => msg(InputChunk::StdinEOF),
            b    => Err(err(&format!("Unrecognized chunk type: {:?}", b))),
        }
    }
}

impl Encoder for Codec {
    type Item = Frame<OutputChunk, (), io::Error>;
    type Error = io::Error;

    fn encode(&mut self, frame: Self::Item, buf: &mut BytesMut) -> io::Result<()> {
      let msg = match frame {
        Frame::Message { message, .. } => message,
        Frame::Body { .. } => unreachable!(),
        Frame::Error { error } => return Err(error),
      };

      // Reserve enough space for the header, and then split into header and chunk.
      buf.reserve(HEADER_SIZE);
      let mut chunk = buf.split_off(HEADER_SIZE);
      let header = buf;

      // Write chunk data into the chunk.
      let msg_type =
        match msg {
          OutputChunk::Pid(pid) => {
            chunk.extend_from_slice(&format!("{}", pid).as_bytes());
            b'P'
          },
          OutputChunk::StartReadingInput => b'S',
          OutputChunk::Stdout(bytes) => {
            chunk.extend_from_slice(&bytes);
            b'1'
          },
          OutputChunk::Stderr(bytes) => {
            chunk.extend_from_slice(&bytes);
            b'2'
          },
          OutputChunk::Exit => b'X',
        };

      // Then write the msg type and body length into the header.
      header.put_u8(msg_type);
      header.put_u32::<BigEndian>(chunk.len() as u32);
      Ok(())
    }
}

fn msg<T>(message: T) -> Result<Option<Frame<T, (), io::Error>>, io::Error> {
    // We're using the tokio `streaming` pattern, but without bodies: all messages are
    // chunked to be small enough to fully decode.
    Ok(
        Some(
            Frame::Message {
                message: message,
                body: false,
            }
        )
    )
}

fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

fn to_string(bytes: &BytesMut) -> Result<String, io::Error> {
    str::from_utf8(bytes).map(|s| s.to_string()).map_err(|e| io::Error::new(io::ErrorKind::Other, e))
}
