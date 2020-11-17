use std::fmt::Debug;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc;
use futures::stream::BoxStream;
use futures::{Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{InputChunk, OutputChunk, ServerCodec};
use crate::execution::{child_channel, send_to_io, Args, ChildInput, ChildOutput, Command, Env};
use crate::{Config, Nail};

pub struct Child {
    pub output_stream: BoxStream<'static, ChildOutput>,
    pub accepts_stdin: bool,
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

    // Create channels for stdio with the forked subprocess, and spawn the process.
    let (stdin_write, stdin_read) = child_channel::<ChildInput>();
    let command_desc = command.command.clone();
    let (accept_stdin, process_read) = match nail.spawn(command, stdin_read) {
        Ok(child) => (child.accepts_stdin, child.output_stream),
        Err(e) => {
            let e = format!("Failed to launch child `{}`: {:?}\n", command_desc, e);
            client_write.send(OutputChunk::Stderr(e.into())).await?;
            client_write.send(OutputChunk::Exit(1)).await?;
            return Ok(());
        }
    };

    // Spawn a task to consume client inputs, which might include any combination of heartbeat and
    // stdin messages.
    let client_write = Arc::new(Mutex::new(client_write));
    let _join = {
        let client_write = client_write.clone();
        tokio::spawn(input(
            config.clone(),
            accept_stdin,
            client_write,
            client_read,
            stdin_write,
        ))
    };

    // Loop writing stdout/stderr to the client.
    output(process_read, client_write).await
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
    mut process_read: BoxStream<'_, ChildOutput>,
    client_write: Arc<Mutex<C>>,
) -> Result<(), io::Error> {
    loop {
        let child_output = match process_read.next().await {
            Some(child_output) => child_output,
            None => {
                // Process exited without sending an exit code... strange, but not fatal.
                break Ok(());
            }
        };

        // We have a valid child output to send.
        let exiting = match child_output {
            ChildOutput::Exit(_) => true,
            _ => false,
        };

        let mut client_write = client_write.lock().await;
        client_write.send(child_output.into()).await?;

        if exiting {
            return Ok(());
        }
    }
}

///
/// Reads client inputs, including heartbeat (optionally validated) and stdin messages (optionally
/// accepted).
///
async fn input<C: ClientSink>(
    config: Config,
    accept_stdin: bool,
    client_write: Arc<Mutex<C>>,
    mut client_read: impl Stream<Item = Result<InputChunk, io::Error>> + Unpin,
    mut process_write: mpsc::Sender<ChildInput>,
) -> Result<(), io::Error> {
    // If the process will accept stdin, send the StartReadingStdin chunk.
    if accept_stdin {
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
                if !accept_stdin {
                    return Err(err(&format!("The StartReadingStdin chunk was not sent: did not expect to receive stdin.")));
                }
                process_write
                    .send(ChildInput::Stdin(bytes))
                    .map_err(send_to_io)
                    .await?;
                // If noisy_stdin is configured, we respond to every new chunk with `StartReadingStdin`.
                if config.noisy_stdin {
                    let mut client_write = client_write.lock().await;
                    client_write.send(OutputChunk::StartReadingStdin).await?;
                }
            }
            InputChunk::StdinEOF => {
                if !accept_stdin {
                    return Err(err(&format!("The StartReadingStdin chunk was not sent: did not expect to receive stdin.")));
                }
                process_write
                    .send(ChildInput::StdinEOF)
                    .map_err(send_to_io)
                    .await?;
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
