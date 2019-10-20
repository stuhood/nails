extern crate bytes;
extern crate futures;
extern crate nails;
extern crate tokio_codec;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;

use std::io;
use std::process::{Command, Stdio};

use bytes::{Bytes, BytesMut};
use futures::sync::mpsc;
use futures::{Future, Sink, Stream};
use tokio_core::reactor::Handle;
use tokio_io::codec::{Decoder, Encoder};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_process::CommandExt;

use nails::execution::{self, send_to_io, unreachable_io, ChildInput, ChildOutput, ExitCode};
use nails::Nail;

/// A Nail implementation that forks processes.
#[derive(Clone)]
pub struct ForkNail;

impl Nail for ForkNail {
    fn spawn(
        &self,
        cmd: execution::Command,
        output_sink: mpsc::Sender<ChildOutput>,
        input_stream: mpsc::Receiver<ChildInput>,
        handle: &Handle,
    ) -> Result<(), io::Error> {
        let mut child = Command::new(cmd.command.clone())
            .args(cmd.args)
            .env_clear()
            .envs(cmd.env)
            .current_dir(cmd.working_dir)
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
                        .map_err(unreachable_io),
                )
                .then(|_| Ok(())),
        );

        // Fully consume the stdout/stderr streams before waiting on the exit stream.
        let stdout_stream = stream_for(child.stdout().take().unwrap())
            .map(|bytes| ChildOutput::Stdout(bytes.into()));
        let stderr_stream = stream_for(child.stderr().take().unwrap())
            .map(|bytes| ChildOutput::Stderr(bytes.into()));
        let exit_stream = child
            .into_stream()
            .map(|exit_status| ChildOutput::Exit(ExitCode(exit_status.code().unwrap_or(-1))));
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
}

fn stream_for<R: AsyncRead + Send + Sized + 'static>(
    r: R,
) -> tokio_codec::FramedRead<R, IdentityCodec> {
    tokio_codec::FramedRead::new(r, IdentityCodec)
}

fn sink_for<W: AsyncWrite + Send + Sized + 'static>(
    w: W,
) -> tokio_codec::FramedWrite<W, IdentityCodec> {
    tokio_codec::FramedWrite::new(w, IdentityCodec)
}

// TODO: Should switch this to a Codec which emits for either lines or elapsed time.
struct IdentityCodec;

impl Decoder for IdentityCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if buf.len() == 0 {
            Ok(None)
        } else {
            Ok(Some(buf.take().freeze()))
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
