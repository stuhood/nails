use std::fmt::Debug;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use futures::channel::mpsc;
use futures::{Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{InputChunk, OutputChunk, ServerCodec};
use crate::execution::{child_channel, send_to_io, Args, ChildInput, ChildOutput, Command, Env};

///
/// Executes the nailgun protocol. Returns success for everything except socket errors.
///
pub async fn execute<R, W, N>(read: R, write: W, config: super::Config<N>) -> Result<(), io::Error>
where
    R: AsyncRead + Debug + Unpin + Send + 'static,
    W: AsyncWrite + Debug + Unpin + Send + 'static,
    N: super::Nail,
{
    // Split the client transport into write and read portions.
    let mut client_read = FramedRead::new(read, ServerCodec);
    let mut client_write = FramedWrite::new(write, ServerCodec);

    // Read the command from the socket.
    let command = match initialize(&mut client_read).await {
        Ok(command) => command,
        Err(e) => {
            client_write.send(OutputChunk::Stderr(e.into())).await?;
            client_write.send(OutputChunk::Exit(1)).await?;
            return Ok(());
        }
    };

    // Create channels for stdio with the forked subprocess, and spawn the process.
    let (process_write, process_read) = child_channel::<ChildOutput>();
    let (stdin_write, stdin_read) = child_channel::<ChildInput>();
    let command_desc = command.command.clone();
    let should_send_stdin = match config.nail.spawn(command, process_write, stdin_read) {
        Ok(should_send_stdin) => should_send_stdin,
        Err(e) => {
            let e = format!("Failed to launch child `{}`: {:?}\n", command_desc, e);
            client_write.send(OutputChunk::Stderr(e.into())).await?;
            client_write.send(OutputChunk::Exit(1)).await?;
            return Ok(());
        }
    };

    // If the Nail indicated that we should read stdin, spawn a task to do so.
    let client_write = Arc::new(Mutex::new(client_write));
    if should_send_stdin {
        let _join = tokio::spawn(stdio_input(
            config.clone(),
            client_write.clone(),
            client_read,
            stdin_write,
        ));
    } else {
        std::mem::drop(stdin_write);
    }

    // Loop writing stdout/stderr to the client.
    stdio_output(process_read, client_write).await
}

///
/// Handles the portion of the protocol before we have received enough arguments to spawn the child
/// process.
///
async fn initialize(
    client_read: &mut (impl Stream<Item = Result<InputChunk, io::Error>> + Unpin),
) -> Result<Command, String> {
    let mut args = Args::new();
    let mut env = Env::new();
    let mut working_dir: Option<PathBuf> = None;
    while let Some(input_chunk) = client_read.next().await {
        let input_chunk =
            input_chunk.map_err(|e| format!("Client error while receiving command: {}", e))?;
        match input_chunk {
            InputChunk::Argument(arg) => args.push(arg),
            InputChunk::Environment { key, val } => env.push((key, val)),
            InputChunk::WorkingDir(w_d) => working_dir = Some(w_d),
            InputChunk::Command(command) => {
                let working_dir = working_dir
                    .ok_or_else(|| format!("Did not receive the required working_dir chunk."))?;
                return Ok(Command {
                    command,
                    args,
                    env,
                    working_dir,
                });
            }
            c => {
                return Err(format!(
                    "The client sent an unexpected chunk during initialization: {:?}",
                    c
                ))
            }
        }
    }
    Err("Client exited before a complete command could be received.".to_string())
}

///
/// Handles reading stdio from the child process and writing it to the client socket.
///
async fn stdio_output<C: ClientSink>(
    mut process_read: mpsc::Receiver<ChildOutput>,
    client_write: Arc<Mutex<C>>,
) -> Result<(), io::Error> {
    while let Some(child_output) = process_read.next().await {
        let exited = match child_output {
            ChildOutput::Exit(_) => true,
            _ => false,
        };
        {
            let mut client_write = client_write.lock().await;
            client_write.send(child_output.into()).await?;
        }
        if exited {
            return Ok(());
        }
    }
    // Process exited without sending an exit code... strange, but not fatal.
    Ok(())
}

///
/// If the Nail has indicated that we should handle stdin, reads it from the socket and sends it to
/// the process.
///
async fn stdio_input<C: ClientSink, N: super::Nail>(
    config: super::Config<N>,
    client_write: Arc<Mutex<C>>,
    mut client_read: impl Stream<Item = Result<InputChunk, io::Error>> + Unpin,
    mut process_write: mpsc::Sender<ChildInput>,
) -> Result<(), io::Error> {
    while let Some(input_chunk) = client_read.next().await {
        match input_chunk? {
            InputChunk::Stdin(bytes) => {
                let noisy_stdin = config.noisy_stdin;
                process_write
                    .send(ChildInput::Stdin(bytes))
                    .map_err(send_to_io)
                    .await?;
                // If noisy_stdin is configured, we respond to every new chunk with `StartReadingStdin`.
                if noisy_stdin {
                    let mut client_write = client_write.lock().await;
                    client_write.send(OutputChunk::StartReadingStdin).await?;
                }
            }
            InputChunk::StdinEOF => {
                process_write
                    .send(ChildInput::StdinEOF)
                    .map_err(send_to_io)
                    .await?;
                break;
            }
            c => {
                return Err(err(&format!(
                    "The client sent an unexpected chunk after initialization: {:?}",
                    c
                )))
            }
        }
    }
    Ok(())
}

pub fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

impl From<ChildOutput> for OutputChunk {
    fn from(co: ChildOutput) -> Self {
        match co {
            ChildOutput::Stdout(bytes) => OutputChunk::Stdout(bytes),
            ChildOutput::Stderr(bytes) => OutputChunk::Stderr(bytes),
            ChildOutput::Exit(code) => OutputChunk::Exit(code.0),
        }
    }
}

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ClientSink: Debug + Sink<OutputChunk, Error = io::Error> + Unpin + Send {}
#[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ClientSink for T where T: Debug + Sink<OutputChunk, Error = io::Error> + Unpin + Send {}
