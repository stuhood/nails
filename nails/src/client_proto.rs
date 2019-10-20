use std::fmt::Debug;
use std::io::{self, Write};

use bytes::Bytes;
use futures::{future, Future, Sink, Stream};
use log::debug;
use tokio_codec::Framed;
use tokio_io::{AsyncRead, AsyncWrite};

use codec::{ClientCodec, InputChunk, OutputChunk};
use execution::{Command, ExitCode};

#[derive(Debug)]
enum Event {
    Server(OutputChunk),
    Cli(CliEvent),
}

#[derive(Debug)]
enum CliEvent {
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug)]
struct ClientState<S: ServerSink>(S);

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
) -> Box<Future<Item = ExitCode, Error = io::Error>>
where
    T: AsyncRead + AsyncWrite + Debug + 'static,
{
    let (server_write, server_read) = transport.split();

    // Send all the init chunks:
    let init_chunks = futures::stream::iter_ok::<_, io::Error>(command_as_chunks(cmd))
        .inspect(|i| debug!("Client sending initialization chunk {:?}", i));

    // Select on the two input sources to create a merged Stream of events.
    // TODO: Handle stdin with this `select`.
    let events_read = futures::stream::empty::<CliEvent, io::Error>()
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
                    .fold(ClientState(server_write), move |state, ev| step(state, ev))
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
        // TODO This blocks because it uses std::io, we should switch it to tokio::io
        (state, Event::Server(OutputChunk::Stderr(bytes))) => {
            debug!("Client got {} bytes of stderr.", bytes.len());
            Box::new(future::result(
                io::stderr()
                    .write_all(&bytes)
                    .map_err(ClientError::Dirty)
                    .map(|()| state),
            ))
        }
        (state, Event::Server(OutputChunk::Stdout(bytes))) => {
            debug!("Client got {} bytes of stdout.", bytes.len());
            Box::new(future::result(
                io::stdout()
                    .write_all(&bytes)
                    .map_err(ClientError::Dirty)
                    .map(|()| state),
            ))
        }
        (state, Event::Server(OutputChunk::StartReadingStdin)) => {
            // TODO: What is the consequence of sending stdin before this chunk? For now we ignore
            // it and send stdin chunks as they arrive.
            Box::new(future::result(Ok(state)))
        }
        (_, Event::Server(OutputChunk::Exit(code))) => {
            debug!("Client got exit code: {}", code);
            Box::new(future::result(Err(ClientError::Clean(ExitCode(code)))))
        }
        (ClientState(server), Event::Cli(CliEvent::Stdin(bytes))) => Box::new(
            server
                .send(InputChunk::Stdin(bytes))
                .map_err(ClientError::Dirty)
                .map(ClientState),
        ),
        (ClientState(server), Event::Cli(CliEvent::StdinEOF)) => Box::new(
            server
                .send(InputChunk::StdinEOF)
                .map_err(ClientError::Dirty)
                .map(ClientState),
        ),
    }
}

fn err(e: &str) -> ClientError {
    ClientError::Dirty(io_err(e))
}

fn io_err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

type ClientFuture<C> = Box<Future<Item = ClientState<C>, Error = ClientError>>;

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ServerSink: Debug + Sink<SinkItem = InputChunk, SinkError = io::Error> + 'static {}
#[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ServerSink for T where T: Debug + Sink<SinkItem = InputChunk, SinkError = io::Error> + 'static {}
