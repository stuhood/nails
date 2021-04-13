use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures::channel::mpsc;
use futures::future::{AbortHandle, Abortable, Aborted, BoxFuture};
use futures::stream::BoxStream;
use futures::{FutureExt, Sink, SinkExt, Stream, StreamExt, TryFutureExt};
use log::{debug, trace};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{ClientCodec, InputChunk, OutputChunk};
use crate::execution::{child_channel, send_to_io, ChildInput, ChildOutput, Command, ExitCode};
use crate::Config;

pub struct Child {
    ///
    /// A stream of outputs from the remote child process.
    ///
    /// Similar to `std::process::Child`, you should `take` this instance to avoid partial moves:
    ///   let output_stream = child.output_stream.take().unwrap();
    ///
    pub output_stream: Option<BoxStream<'static, ChildOutput>>,
    ///
    /// A future for the exit code of the remote process.
    ///
    exit_code: Option<BoxFuture<'static, Result<ExitCode, io::Error>>>,
    ///
    /// A callable to shut down the write half of the connection upon request.
    ///
    shutdown: Option<BoxFuture<'static, ()>>,
    ///
    /// A handle to cancel the background task managing the connection when the Child is dropped.
    ///
    abort_handle: AbortHandle,
}

impl Child {
    ///
    /// Closes the write half of the connection to the server, which will trigger cancellation in
    /// well behaved servers. Because the read half of the connection will still be open, a well
    /// behaved server/Nail will render teardown information before exiting.
    ///
    /// Dropping the Child instance also triggers cancellation, but closes both the read and write
    /// halves of the connection at the same time (which does not allow for orderly shutdown of
    /// the server).
    ///
    pub async fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.await;
        }
    }

    ///
    /// Wait for the Child to have exited, and return an ExitCode.
    ///
    pub async fn wait(mut self) -> Result<ExitCode, io::Error> {
        // This method may only be called once, so it's safe to take the exit code unconditionally.
        self.exit_code.take().unwrap().await
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        self.abort_handle.abort();
    }
}

///
/// Implements the client side of a single connection on the given socket.
///
/// The `input_stream` is lazily instantiated because servers only optionally accept input, and
/// clients should not begin reading stdin from their callers unless the server will accept it.
///
pub async fn handle_connection(
    config: Config,
    socket: TcpStream,
    cmd: Command,
    open_input_stream: impl Future<Output = mpsc::Receiver<ChildInput>> + Send + 'static,
) -> Result<Child, io::Error> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.into_split();
    execute(config, read, write, cmd, open_input_stream).await
}

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

async fn execute<R, W>(
    config: Config,
    read: R,
    write: W,
    cmd: Command,
    open_cli_read: impl Future<Output = mpsc::Receiver<ChildInput>> + Send + 'static,
) -> Result<Child, io::Error>
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
    let server_write = Arc::new(Mutex::new(Some(server_write)));

    // Calls to shutdown will drop the write half of the socket.
    let shutdown = {
        let server_write = server_write.clone();
        async move {
            // Take and drop the write half of the connection (if it has not already been dropped).
            let _ = server_write.lock().await.take();
        }
    };

    // If configured, spawn a task to send heartbeats.
    if let Some(heartbeat_frequency) = config.heartbeat_frequency {
        let _join = tokio::spawn(heartbeat_sender(
            Arc::downgrade(&server_write),
            heartbeat_frequency,
        ));
    }

    // Then handle stdio until we receive an ExitCode, or until the Child is dropped.
    let (cli_write, cli_read) = child_channel::<ChildOutput>();
    let (abort_handle, exit_code) = {
        // We spawn the execution of the process onto a background task to ensure that it starts
        // running even if a consumer of the Child instance chooses to completely consume the stdio
        // output_stream before interacting with the exit code (rathering than `join`ing them).
        //
        // We wrap in Abortable so that dropping the Child instance cancels the background task.
        let (abort_handle, abort_registration) = AbortHandle::new_pair();
        let stdio_task = handle_stdio(server_read, server_write.clone(), cli_write, open_cli_read);
        let exit_code_result = tokio::spawn(Abortable::new(stdio_task, abort_registration));
        let exit_code = async move {
            match exit_code_result.await.unwrap() {
                Ok(res) => res,
                Err(Aborted) => Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "The connection was canceled because the Child was dropped",
                )),
            }
        }
        .boxed();
        (abort_handle, exit_code)
    };
    Ok(Child {
        output_stream: Some(cli_read.boxed()),
        exit_code: Some(exit_code),
        shutdown: Some(shutdown.boxed()),
        abort_handle,
    })
}

async fn handle_stdio<S: ServerSink>(
    mut server_read: impl Stream<Item = Result<OutputChunk, io::Error>> + Unpin,
    server_write: Arc<Mutex<Option<S>>>,
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
                return Ok(ExitCode(code));
            }
        }
    }
    Err(io_err(
        "Client exited before the server's result could be returned.",
    ))
}

async fn stdin_sender<S: ServerSink>(
    server_write: Arc<Mutex<Option<S>>>,
    mut cli_read: mpsc::Receiver<ChildInput>,
) -> Result<(), io::Error> {
    while let Some(input_chunk) = cli_read.next().await {
        if let Some(ref mut server_write) = *server_write.lock().await {
            match input_chunk {
                ChildInput::Stdin(bytes) => {
                    trace!("nails client sending {} bytes of stdin.", bytes.len());
                    server_write.send(InputChunk::Stdin(bytes)).await?;
                }
            }
        } else {
            break;
        };
    }

    if let Some(ref mut server_write) = *server_write.lock().await {
        server_write.send(InputChunk::StdinEOF).await?;
    }
    Ok(())
}

async fn heartbeat_sender<S: ServerSink>(
    server_write: Weak<Mutex<Option<S>>>,
    heartbeat_frequency: Duration,
) -> Result<(), io::Error> {
    loop {
        // Wait a fraction of the desired frequency (which from a client's perspective is a
        // minimum: more frequent is fine).
        tokio::time::sleep(heartbeat_frequency / 4).await;

        // Then, if the connection might still be alive...
        if let Some(server_write) = server_write.upgrade() {
            let mut server_write = server_write.lock().await;
            if let Some(ref mut server_write) = *server_write {
                server_write.send(InputChunk::Heartbeat).await?;
            } else {
                break Ok(());
            }
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
trait ServerSink: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send + 'static {}
impl<T> ServerSink for T where
    T: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send + 'static
{
}
