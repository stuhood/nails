use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::channel::mpsc;
use futures::{Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use log::{debug, trace};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio::time::delay_for;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{ClientCodec, InputChunk, OutputChunk};
use crate::execution::{send_to_io, ChildInput, ChildOutput, Command, ExitCode};
use crate::Config;

///
/// Converts a Command into the initialize chunks for the nailgun protocol. Note: order matters.
///
fn command_as_chunks(cmd: Command) -> Vec<InputChunk> {
    let Command {
        command,
        args,
        env,
        working_dir,
    } = cmd;

    let mut chunks = Vec::new();
    chunks.extend(args.into_iter().map(InputChunk::Argument));
    chunks.extend(
        env.into_iter()
            .map(|(key, val)| InputChunk::Environment { key, val }),
    );
    chunks.push(InputChunk::WorkingDir(working_dir));
    chunks.push(InputChunk::Command(command));
    chunks
}

pub async fn execute<R, W>(
    config: Config,
    read: R,
    write: W,
    cmd: Command,
    cli_write: mpsc::Sender<ChildOutput>,
    open_cli_read: impl Future<Output = mpsc::Receiver<ChildInput>>,
) -> Result<ExitCode, io::Error>
where
    R: AsyncRead + Debug + Unpin + Send + 'static,
    W: AsyncWrite + Debug + Unpin + Send + 'static,
{
    let server_read = FramedRead::new(read, ClientCodec);
    let mut server_write = FramedWrite::new(write, ClientCodec);

    // Send all of the init chunks.
    let mut init_chunks = futures::stream::iter(command_as_chunks(cmd).into_iter().map(Ok))
        .inspect(|i| debug!("nails client sending initialization chunk {:?}", i));
    server_write
        .send_all(&mut init_chunks)
        .map_err(|e| {
            io_err(&format!(
                "Could not send initial chunks to the server. Got: {}",
                e
            ))
        })
        .await?;

    // If configured, spawn a task to send heartbeats.
    let server_write = Arc::new(Mutex::new(server_write));
    if let Some(heartbeat_frequency) = config.heartbeat_frequency {
        let _join = tokio::spawn(heartbeat_sender(
            Arc::downgrade(&server_write),
            heartbeat_frequency,
        ));
    }

    // Then handle stdio until we receive an ExitCode.
    let exit_code_res =
        handle_stdio(server_read, server_write.clone(), cli_write, open_cli_read).await;
    // TODO: Closing the write half of the `into_split` socket seemingly closes the entire socket,
    // so we hold onto it here until the rest of the protocol has completed.
    std::mem::drop(server_write);
    exit_code_res
}

async fn handle_stdio<S: ServerSink>(
    mut server_read: impl Stream<Item = Result<OutputChunk, io::Error>> + Unpin,
    server_write: Arc<Mutex<S>>,
    mut cli_write: mpsc::Sender<ChildOutput>,
    open_cli_read: impl Future<Output = mpsc::Receiver<ChildInput>>,
) -> Result<ExitCode, io::Error> {
    let mut stdin_inputs = Some((server_write, open_cli_read));
    while let Some(output_chunk) = server_read.next().await {
        match output_chunk? {
            OutputChunk::Stderr(bytes) => {
                trace!("nails client got {} bytes of stderr.", bytes.len());
                cli_write
                    .send(ChildOutput::Stderr(bytes))
                    .map_err(|e| send_to_io(e))
                    .await?;
            }
            OutputChunk::Stdout(bytes) => {
                trace!("nails client got {} bytes of stdout.", bytes.len());
                cli_write
                    .send(ChildOutput::Stdout(bytes))
                    .map_err(|e| send_to_io(e))
                    .await?;
            }
            OutputChunk::StartReadingStdin => {
                // We spawn a task to send stdin after receiving `StartReadingStdin`, but only
                // once: some servers (ours included, optionally) have a `noisy_stdin` behaviour
                // where they ask for more input after every Stdin chunk.
                if let Some((server_write, open_cli_read)) = stdin_inputs.take() {
                    debug!("nails client will start sending stdin.");
                    let _join = tokio::spawn(stdin_sender(server_write, open_cli_read.await));
                }
            }
            OutputChunk::Exit(code) => {
                trace!("nails client got exit code: {}", code);
                let code = ExitCode(code);
                cli_write
                    .send(ChildOutput::Exit(code))
                    .map_err(|e| send_to_io(e))
                    .await?;
                return Ok(code);
            }
        }
    }
    Err(io_err(
        "Client exited before the server's result could be returned.",
    ))
}

async fn stdin_sender<S: ServerSink>(
    server_write: Arc<Mutex<S>>,
    mut cli_read: mpsc::Receiver<ChildInput>,
) -> Result<(), io::Error> {
    while let Some(input_chunk) = cli_read.next().await {
        match input_chunk {
            ChildInput::Stdin(bytes) => {
                trace!("nails client sending {} bytes of stdin.", bytes.len());
                let mut server_write = server_write.lock().await;
                server_write.send(InputChunk::Stdin(bytes)).await?;
            }
            ChildInput::StdinEOF => {
                trace!("nails client closing stdin.");
                let mut server_write = server_write.lock().await;
                server_write.send(InputChunk::StdinEOF).await?;
                break;
            }
        }
    }
    Ok(())
}

async fn heartbeat_sender<S: ServerSink>(
    server_write: Weak<Mutex<S>>,
    heartbeat_frequency: Duration,
) -> Result<(), io::Error> {
    loop {
        // Wait a fraction of the desired frequency (which from a client's perspective is a
        // minimum: more frequent is fine).
        delay_for(heartbeat_frequency / 4).await;

        // Then, if the connection might still be alive...
        if let Some(server_write) = server_write.upgrade() {
            let mut server_write = server_write.lock().await;
            server_write.send(InputChunk::Heartbeat).await?;
        } else {
            break Ok(());
        };
    }
}

fn io_err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ServerSink: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send + 'static {}
#[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ServerSink for T where T: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send + 'static {}
