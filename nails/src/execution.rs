use futures;

use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use bytes::{Bytes, BytesMut};
use futures::channel::mpsc;
use futures::{Sink, Stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{self, Decoder, Encoder};

const BUF_COUNT: usize = 128;

pub type Args = Vec<String>;
pub type Env = Vec<(String, String)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(pub i32);

#[derive(Debug, Default)]
pub struct Command {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChildInput {
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(ExitCode),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(BUF_COUNT)
}

pub fn send_to_io(e: mpsc::SendError) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        format!("Failed to send: {:?}", e),
    )
}

pub fn stream_for<R: AsyncRead + Send + Sized>(
    r: R,
) -> impl Stream<Item = Result<Bytes, io::Error>> {
    codec::FramedRead::new(r, IdentityCodec)
}

pub fn sink_for<W: AsyncWrite + Send + Sized>(w: W) -> impl Sink<Bytes> {
    codec::FramedWrite::new(w, IdentityCodec)
}

// See https://github.com/rust-lang/futures-rs/issues/2006#issuecomment-623054254.
struct IdentityCodec;

impl Decoder for IdentityCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.len() == 0 {
            Ok(None)
        } else {
            Ok(Some(buf.split().freeze()))
        }
    }
}

impl Encoder for IdentityCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&mut self, item: Bytes, buf: &mut BytesMut) -> Result<(), io::Error> {
        if item.len() > 0 {
            buf.extend(item);
        }
        Ok(())
    }
}
