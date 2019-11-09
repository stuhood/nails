use std::fmt::Debug;
use std::io;

use futures::channel::mpsc;
use futures::{stream, Sink, SinkExt, StreamExt, TryFutureExt, TryStreamExt};
use log::debug;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{ClientCodec, InputChunk, OutputChunk};
use crate::execution::{send_to_io, ChildInput, ChildOutput, Command, ExitCode};

#[derive(Debug)]
enum Event {
    Server(OutputChunk),
    Cli(ChildInput),
}

#[derive(Debug)]
struct ClientState<S: ServerSink>(S, mpsc::Sender<ChildOutput>);

///
/// Exiting via an exit code is an error condition, but we can only exit a `fold` via an error.
///
/// TODO: Why isn't there a loop_fn equivalent on Stream?
///
enum ClientError {
    Dirty(io::Error),
    Clean(ExitCode),
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

pub async fn execute<R, W>(
    read: R,
    write: W,
    cmd: Command,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
) -> Result<ExitCode, io::Error>
where
    R: AsyncRead + Debug + Unpin + Send,
    W: AsyncWrite + Debug + Unpin + Send,
{
    let server_read = FramedRead::new(read, ClientCodec);
    let mut server_write = FramedWrite::new(write, ClientCodec);

    // Send all the init chunks:
    let mut init_chunks = futures::stream::iter(command_as_chunks(cmd).into_iter().map(Ok))
        .inspect(|i| debug!("Client sending initialization chunk {:?}", i));

    // Select on the two input sources to create a merged Stream of events.
    let mut events_read = stream::select(
        input_stream.map(|v| Ok(Event::Cli(v))),
        server_read.map_ok(|e| Event::Server(e)),
    );

    server_write
        .send_all(&mut init_chunks)
        .map_err(|e| {
            io_err(&format!(
                "Could not send initial chunks to the server. Got: {}",
                e
            ))
        })
        .await?;

    let mut state = ClientState(server_write, output_sink);
    while let Some(event) = events_read.next().await {
        state = match step(state, event?).await {
            Ok(s) => s,
            Err(ClientError::Clean(code)) => return Ok(code),
            Err(ClientError::Dirty(e)) => return Err(e),
        };
    }
    Err(io_err(
        "Client exited before the server's result could be returned.",
    ))
}

async fn step<C: ServerSink>(
    state: ClientState<C>,
    ev: Event,
) -> Result<ClientState<C>, ClientError> {
    match (state, ev) {
        (ClientState(server, mut cli), Event::Server(OutputChunk::Stderr(bytes))) => {
            debug!("Client got {} bytes of stderr.", bytes.len());
            cli.send(ChildOutput::Stderr(bytes))
                .map_err(|e| ClientError::Dirty(send_to_io(e)))
                .await?;
            Ok(ClientState(server, cli))
        }
        (ClientState(server, mut cli), Event::Server(OutputChunk::Stdout(bytes))) => {
            debug!("Client got {} bytes of stdout.", bytes.len());
            cli.send(ChildOutput::Stdout(bytes))
                .map_err(|e| ClientError::Dirty(send_to_io(e)))
                .await?;
            Ok(ClientState(server, cli))
        }
        (state, Event::Server(OutputChunk::StartReadingStdin)) => {
            // TODO: What is the consequence of sending stdin before this chunk? For now we ignore
            // it and send stdin chunks as they arrive.
            Ok(state)
        }
        (ClientState(_, mut cli), Event::Server(OutputChunk::Exit(code))) => {
            debug!("Client got exit code: {}", code);
            let code = ExitCode(code);
            cli.send(ChildOutput::Exit(code))
                .map_err(|e| ClientError::Dirty(send_to_io(e)))
                .await?;
            Err(ClientError::Clean(code))
        }
        (ClientState(mut server, cli), Event::Cli(ChildInput::Stdin(bytes))) => {
            server
                .send(InputChunk::Stdin(bytes))
                .map_err(ClientError::Dirty)
                .await?;
            Ok(ClientState(server, cli))
        }
        (ClientState(mut server, cli), Event::Cli(ChildInput::StdinEOF)) => {
            server
                .send(InputChunk::StdinEOF)
                .map_err(ClientError::Dirty)
                .await?;
            Ok(ClientState(server, cli))
        }
    }
}

fn io_err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ServerSink: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send {}
#[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ServerSink for T where T: Debug + Sink<InputChunk, Error = io::Error> + Unpin + Send {}
