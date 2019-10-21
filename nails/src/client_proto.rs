use std::fmt::Debug;
use std::io;

use futures::sync::mpsc;
use futures::{future, Future, Sink, Stream};
use log::debug;
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};

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

pub fn execute<T>(
    transport: Framed<T, ClientCodec>,
    cmd: Command,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
) -> Box<dyn Future<Item = ExitCode, Error = io::Error>>
where
    T: AsyncRead + AsyncWrite + Debug + 'static,
{
    let (server_write, server_read) = transport.split();

    // Send all the init chunks:
    let init_chunks = futures::stream::iter_ok::<_, io::Error>(command_as_chunks(cmd))
        .inspect(|i| debug!("Client sending initialization chunk {:?}", i));

    // Select on the two input sources to create a merged Stream of events.
    let events_read = input_stream
        .then(|res| match res {
            Ok(v) => Ok(Event::Cli(v)),
            Err(e) => Err(err(&format!("Failed to emit child output: {:?}", e))),
        })
        .select(
            server_read
                .map(|e| Event::Server(e))
                .map_err(ClientError::Dirty),
        );

    Box::new(
        server_write
            .send_all(init_chunks)
            .map_err(|e| {
                io_err(&format!(
                    "Could not send initial chunks to the server. Got: {}",
                    e
                ))
            })
            .and_then(|sink_and_stream| {
                let (server_write, _) = sink_and_stream;
                events_read
                    .fold(ClientState(server_write, output_sink), move |state, ev| {
                        step(state, ev)
                    })
                    .then(|res| match res {
                        Err(ClientError::Clean(code)) => Ok(code),
                        Ok(_) => Err(io_err(
                            "Client exited before the server's result could be returned.",
                        )),
                        Err(ClientError::Dirty(e)) => Err(e),
                    })
            }),
    )
}

fn step<C: ServerSink>(state: ClientState<C>, ev: Event) -> ClientFuture<C> {
    match (state, ev) {
        (ClientState(server, cli), Event::Server(OutputChunk::Stderr(bytes))) => {
            debug!("Client got {} bytes of stderr.", bytes.len());
            Box::new(
                cli.send(ChildOutput::Stderr(bytes))
                    .map_err(|e| ClientError::Dirty(send_to_io(e)))
                    .map(|cli| ClientState(server, cli)),
            )
        }
        (ClientState(server, cli), Event::Server(OutputChunk::Stdout(bytes))) => {
            debug!("Client got {} bytes of stdout.", bytes.len());
            Box::new(
                cli.send(ChildOutput::Stdout(bytes))
                    .map_err(|e| ClientError::Dirty(send_to_io(e)))
                    .map(|cli| ClientState(server, cli)),
            )
        }
        (state, Event::Server(OutputChunk::StartReadingStdin)) => {
            // TODO: What is the consequence of sending stdin before this chunk? For now we ignore
            // it and send stdin chunks as they arrive.
            Box::new(future::result(Ok(state)))
        }
        (ClientState(_, cli), Event::Server(OutputChunk::Exit(code))) => {
            debug!("Client got exit code: {}", code);
            let code = ExitCode(code);
            Box::new(
                cli.send(ChildOutput::Exit(code))
                    .map_err(|e| ClientError::Dirty(send_to_io(e)))
                    .and_then(move |_| Err(ClientError::Clean(code))),
            )
        }
        (ClientState(server, cli), Event::Cli(ChildInput::Stdin(bytes))) => Box::new(
            server
                .send(InputChunk::Stdin(bytes))
                .map_err(ClientError::Dirty)
                .map(|server| ClientState(server, cli)),
        ),
        (ClientState(server, cli), Event::Cli(ChildInput::StdinEOF)) => Box::new(
            server
                .send(InputChunk::StdinEOF)
                .map_err(ClientError::Dirty)
                .map(|server| ClientState(server, cli)),
        ),
    }
}

fn err(e: &str) -> ClientError {
    ClientError::Dirty(io_err(e))
}

fn io_err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

type ClientFuture<C> = Box<dyn Future<Item = ClientState<C>, Error = ClientError>>;

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ServerSink: Debug + Sink<SinkItem = InputChunk, SinkError = io::Error> + 'static {}
#[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ServerSink for T where T: Debug + Sink<SinkItem = InputChunk, SinkError = io::Error> + 'static {}
