use std::fmt::Debug;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable, Aborted, BoxFuture};
use futures::stream::BoxStream;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{InputChunk, OutputChunk, ServerCodec};
use crate::execution::{send_to_io, Args, ChildInput, ChildOutput, Command, Env, ExitCode};
use crate::{Config, Nail};

pub struct Child {
    ///
    /// A stream of outputs from the local child process.
    ///
    /// Similar to `std::process::Child`, you should `take` this instance to avoid partial moves:
    ///   let output_stream = child.output_stream.take().unwrap();
    ///
    output_stream: Option<BoxStream<'static, Result<ChildOutput, io::Error>>>,
    ///
    /// If the Nail implementation accepts stdin, a sink for stdin.
    ///
    /// Similar to `std::process::Child`, you should `take` this instance to avoid partial moves:
    ///   let input_sink = child.input_sink.take().unwrap();
    ///
    input_sink: Option<mpsc::Sender<ChildInput>>,
    ///
    /// A future for the exit code of the local process. The server guarantees to `spawn` this
    /// future, and to cancel it on errors interacting with the socket.
    ///
    exit_code: Option<BoxFuture<'static, Result<ExitCode, io::Error>>>,
    ///
    /// A callable that indicates that the client has attempted a clean shutdown of this connection.
    ///
    shutdown: Option<BoxFuture<'static, ()>>,
}

impl Child {
    pub fn new(
        output_stream: BoxStream<'static, Result<ChildOutput, io::Error>>,
        input_sink: Option<mpsc::Sender<ChildInput>>,
        exit_code: BoxFuture<'static, Result<ExitCode, io::Error>>,
        shutdown: Option<BoxFuture<'static, ()>>,
    ) -> Child {
        Child {
            output_stream: Some(output_stream),
            input_sink,
            exit_code: Some(exit_code),
            shutdown,
        }
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

///
/// Implements the server side of a single connection on the given socket.
///
pub async fn handle_connection(
    config: Config,
    nail: impl Nail,
    socket: TcpStream,
) -> Result<(), io::Error> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.into_split();
    execute(config, nail, read, write).await
}

///
/// Executes the nailgun protocol. Returns success for everything except socket errors.
///
async fn execute<R, W>(config: Config, nail: impl Nail, read: R, write: W) -> Result<(), io::Error>
where
    R: AsyncRead + Debug + Unpin + Send + 'static,
    W: AsyncWrite + Debug + Unpin + Send + 'static,
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

    // Spawn the process.
    let command_desc = command.command.clone();
    let mut child = match nail.spawn(command) {
        Ok(child) => child,
        Err(e) => {
            let e = format!("Failed to launch child `{}`: {:?}\n", command_desc, e);
            client_write.send(OutputChunk::Stderr(e.into())).await?;
            client_write.send(OutputChunk::Exit(1)).await?;
            return Ok(());
        }
    };
    let process_read = child.output_stream.take().unwrap();
    let stdin_write = child.input_sink.take();
    let shutdown = child.shutdown.take();

    // Spawn a task to consume client inputs, which might include any combination of heartbeat and
    // stdin messages.
    let client_write = Arc::new(Mutex::new(client_write));
    let input_task = {
        let client_write = client_write.clone();
        tokio::spawn(input(
            config.clone(),
            client_write,
            client_read,
            stdin_write,
            shutdown,
        ))
    };

    // Spawn the nail itself, wrapped in an Abortable.
    let (nail_task, _abort_on_drop) = {
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let nail_task = tokio::spawn(
            Abortable::new(child.exit_code.take().unwrap(), abort_registration).map(
                |res| match res {
                    Ok(res) => res,
                    Err(Aborted) => Ok(ExitCode(-1)),
                },
            ),
        );
        (nail_task, AbortOnDrop(abort_handle))
    };

    // Loop writing stdout/stderr to the client, then join the input task.
    output(process_read, &client_write).await?;
    input_task.await??;

    // Finally, await and send the exit code.
    let exit_code = nail_task.await??;
    let mut client_write = client_write.lock().await;
    client_write.send(OutputChunk::Exit(exit_code.0)).await
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
            InputChunk::Heartbeat => {}
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
async fn output<C: ClientSink>(
    mut process_read: BoxStream<'_, Result<ChildOutput, io::Error>>,
    client_write: &Arc<Mutex<C>>,
) -> Result<(), io::Error> {
    while let Some(child_output) = process_read.next().await {
        let mut client_write = client_write.lock().await;
        client_write.send(child_output?.into()).await?;
    }
    Ok(())
}

///
/// Reads client inputs, including heartbeat (optionally validated) and stdin messages (optionally
/// accepted).
///
async fn input<C: ClientSink>(
    config: Config,
    client_write: Arc<Mutex<C>>,
    mut client_read: impl Stream<Item = Result<InputChunk, io::Error>> + Unpin,
    mut process_write: Option<mpsc::Sender<ChildInput>>,
    shutdown: Option<BoxFuture<'static, ()>>,
) -> Result<(), io::Error> {
    // If the process will accept stdin, send the StartReadingStdin chunk.
    if process_write.is_some() {
        let mut client_write = client_write.lock().await;
        client_write.send(OutputChunk::StartReadingStdin).await?;
    }

    let res = loop {
        let input_chunk =
            match read_client_chunk(&mut client_read, config.heartbeat_frequency).await {
                Some(Ok(input_chunk)) => input_chunk,
                Some(Err(e)) => break Err(e),
                None => break Ok(()),
            };

        // We have a valid chunk.
        match input_chunk {
            InputChunk::Stdin(bytes) => {
                if let Some(ref mut process_write) = process_write.as_mut() {
                    process_write
                        .send(ChildInput::Stdin(bytes))
                        .map_err(send_to_io)
                        .await?;
                } else {
                    return Err(err(&format!(
                        "The StartReadingStdin chunk was not sent, or Stdin was already closed."
                    )));
                }
                // If noisy_stdin is configured, we respond to every new chunk with `StartReadingStdin`.
                if config.noisy_stdin {
                    let mut client_write = client_write.lock().await;
                    client_write.send(OutputChunk::StartReadingStdin).await?;
                }
            }
            InputChunk::StdinEOF => {
                // Drop the stdin Sink.
                if let None = process_write.take() {
                    return Err(err(&format!("The StartReadingStdin chunk was not sent: did not expect to receive stdin.")));
                }
            }
            InputChunk::Heartbeat => {}
            c => {
                return Err(err(&format!(
                    "The client sent an unexpected chunk after initialization: {:?}",
                    c
                )));
            }
        }
    };

    // The input stream is closed, or heartbeats did not arrive in time. Trigger shutdown.
    if let Some(shutdown) = shutdown {
        shutdown.await;
    }

    res
}

///
/// Read a single chunk from the client, optionally applying a heartbeat frequency (ie, timeout).
/// Any message at all is sufficient to reset the clock on the heartbeat.
///
/// None indicates a cleanly closed connection.
///
async fn read_client_chunk(
    client_read: &mut (impl Stream<Item = Result<InputChunk, io::Error>> + Unpin),
    require_heartbeat_frequency: Option<Duration>,
) -> Option<Result<InputChunk, io::Error>> {
    if let Some(per_msg_timeout) = require_heartbeat_frequency {
        match timeout(per_msg_timeout, client_read.next()).await {
            Ok(opt) => opt,
            Err(_) => Some(Err(err(&format!(
                "Did not receive a heartbeat (or any other message) within {:?}",
                per_msg_timeout
            )))),
        }
    } else {
        client_read.next().await
    }
}

pub fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

impl From<ChildOutput> for OutputChunk {
    fn from(co: ChildOutput) -> Self {
        match co {
            ChildOutput::Stdout(bytes) => OutputChunk::Stdout(bytes),
            ChildOutput::Stderr(bytes) => OutputChunk::Stderr(bytes),
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
