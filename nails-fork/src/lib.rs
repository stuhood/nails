use std::io;
use std::process::Stdio;

use bytes::{Bytes, BytesMut};
use futures::channel::mpsc;
use futures::{future, stream, FutureExt, SinkExt, StreamExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio_util::codec::{self, Decoder, Encoder};

use nails::execution::{self, send_to_io, ChildInput, ChildOutput, ExitCode};
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
    ) -> Result<(), io::Error> {
        let mut child = Command::new(cmd.command.clone())
            .args(cmd.args)
            .env_clear()
            .envs(cmd.env)
            .current_dir(cmd.working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()?;

        let mut bounded_input_stream = input_stream
            .take_while(|child_input| match child_input {
                &ChildInput::Stdin(_) => future::ready(true),
                &ChildInput::StdinEOF => future::ready(false),
            })
            .map(|child_input| match child_input {
                ChildInput::Stdin(bytes) => Ok(bytes),
                ChildInput::StdinEOF => unreachable!(),
            });

        // Copy inputs to the child.
        let stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            sink_for(stdin)
                .send_all(&mut bounded_input_stream)
                .map(|_| ())
                .await;
        });

        // Fully consume the stdout/stderr streams before waiting on the exit stream.
        let stdout_stream = stream_for(child.stdout.take().unwrap())
            .map_ok(|bytes| ChildOutput::Stdout(bytes.into()));
        let stderr_stream = stream_for(child.stderr.take().unwrap())
            .map_ok(|bytes| ChildOutput::Stderr(bytes.into()));
        let exit_stream = child
            .into_stream()
            .map_ok(|exit_status| ChildOutput::Exit(ExitCode(exit_status.code().unwrap_or(-1))));
        let mut output_stream = stream::select(stdout_stream, stderr_stream).chain(exit_stream);

        // Spawn a task to send all of stdout/sterr/exit to our output sink.
        tokio::spawn(async move {
            output_sink
                .sink_map_err(send_to_io)
                .send_all(&mut output_stream)
                .map(|_| ())
                .await;
        });

        Ok(())
    }
}

fn stream_for<R: AsyncRead + Send + Sized>(r: R) -> codec::FramedRead<R, IdentityCodec> {
    codec::FramedRead::new(r, IdentityCodec)
}

fn sink_for<W: AsyncWrite + Send + Sized>(w: W) -> codec::FramedWrite<W, IdentityCodec> {
    codec::FramedWrite::new(w, IdentityCodec)
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
