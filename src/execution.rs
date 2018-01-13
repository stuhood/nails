use std::fmt::Debug;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bytes::{Bytes, BytesMut};
use futures::{Future, Stream, Sink};
use futures::sync::mpsc;
use tokio_core::reactor::Handle;
use tokio_io::{self, AsyncRead, AsyncWrite};
use tokio_process::CommandExt;
use tokio_io::codec::Encoder;

const BUF_COUNT: usize = 128;

#[derive(Debug, Default)]
pub struct Args {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum ChildInput {
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(BUF_COUNT)
}

pub fn send_to_io<T: Debug>(e: mpsc::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("Failed to send: {:?}", e))
}

fn unreachable_io() -> io::Error {
    unreachable!()
}

pub fn spawn(
    cmd: String,
    args: Args,
    working_dir: PathBuf,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
    handle: &Handle,
) -> Result<(), io::Error> {
    let mut child = Command::new(cmd.clone())
        .args(args.args)
        .env_clear()
        .envs(args.env)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn_async(handle)?;

    // Copy inputs to the child.
    handle.spawn(
        sink_for(child.stdin().take().unwrap())
            .send_all(
                input_stream
                    .take_while(|child_input| match child_input {
                        &ChildInput::Stdin(_) => Ok(true),
                        &ChildInput::StdinEOF => Ok(false),
                    })
                    .map(|child_input| match child_input {
                        ChildInput::Stdin(bytes) => bytes,
                        ChildInput::StdinEOF => unreachable!(),
                    })
                    .map_err(|_| unreachable_io()),
            )
            .then(|_| Ok(())),
    );

    // Fully consume the stdout/stderr streams before waiting on the exit stream.
    let stdout_stream =
        stream_for(child.stdout().take().unwrap()).map(|bytes| ChildOutput::Stdout(bytes.into()));
    let stderr_stream =
        stream_for(child.stderr().take().unwrap()).map(|bytes| ChildOutput::Stderr(bytes.into()));
    let exit_stream = child.into_stream().map(|exit_status| {
        ChildOutput::Exit(exit_status.code().unwrap_or(-1))
    });
    let output_stream = stdout_stream.select(stderr_stream).chain(exit_stream);

    // Spawn a task to send all of stdout/sterr/exit to our output sink.
    handle.spawn(
        output_sink
            .sink_map_err(send_to_io)
            .send_all(output_stream)
            .then(|_| Ok(())),
    );

    Ok(())
}

fn stream_for<R: AsyncRead + Send + Sized + 'static>(r: R) -> tokio_io::io::Lines<BufReader<R>> {
    // TODO: Should switch this to a Codec which emits for either lines or elapsed time.
    // TODO: This is stripping newlines.
    tokio_io::io::lines(io::BufReader::new(r))
}

fn sink_for<W: AsyncWrite + Send + Sized + 'static>(
    w: W,
) -> tokio_io::codec::FramedWrite<W, IdentityCodec> {
    tokio_io::codec::FramedWrite::new(w, IdentityCodec)
}

struct IdentityCodec;

impl Encoder for IdentityCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn encode(&mut self, item: Bytes, buf: &mut BytesMut) -> Result<(), io::Error> {
        buf.extend(item);
        Ok(())
    }
}
