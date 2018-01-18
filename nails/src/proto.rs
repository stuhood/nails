use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::{future, Future, Stream, Sink};
use futures::sync::mpsc;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::Framed;
use tokio_core::reactor::Handle;

use codec::{Codec, InputChunk, OutputChunk};
use execution::{Args, ChildInput, ChildOutput, child_channel, send_to_io};

#[derive(Debug)]
enum State<C: ClientSink> {
    // While we're gathering arguments and environment, but before the working directory has
    // arrived.
    Initializing(C, mpsc::Sender<ChildOutput>, Args),
    // After the working directory has arrived, but before the command arrives.
    PreCommand(C, mpsc::Sender<ChildOutput>, Args, PathBuf),
    // Executing, and able to receive stdin.
    Executing(C, mpsc::Sender<ChildInput>),
    // Process has finished executing.
    Exited(i32),
}

#[derive(Debug)]
enum Event {
    Client(InputChunk),
    Process(ChildOutput),
}

pub fn execute<T, N>(
    handle: Handle,
    transport: Framed<T, Codec>,
    config: super::Config<N>,
) -> IOFuture<()>
where
    T: AsyncRead + AsyncWrite + Debug + 'static,
    N: super::Nail,
{
    // Create a channel to consume process output from a forked subprocess, and split the client
    // transport into write and read portions.
    let (process_write, process_read) = child_channel::<ChildOutput>();
    let (client_write, client_read) = transport.split();

    // Select on the two input sources to create a merged Stream of events.
    let events_read = process_read
        .then(|res| match res {
            Ok(v) => Ok(Event::Process(v)),
            Err(e) => Err(err(&format!("Failed to emit child output: {:?}", e))),
        })
        .select(client_read.map(|e| Event::Client(e)));

    Box::new(
        events_read
            .fold(
                State::Initializing(client_write, process_write, Default::default()),
                move |state, ev| step(&handle, &config, state, ev),
            )
            .then(|_| Ok(())),
    )
}

fn step<C: ClientSink, N: super::Nail>(
    handle: &Handle,
    config: &super::Config<N>,
    state: State<C>,
    ev: Event,
) -> IOFuture<State<C>> {
    match (state, ev) {
        (State::Initializing(c, p, mut args), Event::Client(InputChunk::Argument(arg))) => {
            args.args.push(arg);
            ok(State::Initializing(c, p, args))
        }
        (State::Initializing(c, p, mut args),
         Event::Client(InputChunk::Environment { key, val })) => {
            args.env.push((key, val));
            ok(State::Initializing(c, p, args))
        }
        (State::Initializing(c, p, args), Event::Client(InputChunk::WorkingDir(working_dir))) => {
            ok(State::PreCommand(c, p, args, working_dir))
        }
        (State::PreCommand(client, output_sink, args, working_dir),
         Event::Client(InputChunk::Command(cmd))) => {
            let cmd_desc = cmd.clone();
            let (stdin_tx, stdin_rx) = child_channel::<ChildInput>();
            let spawn_res = config.nail.spawn(
                cmd,
                args,
                working_dir,
                output_sink,
                stdin_rx,
                handle,
            );
            Box::new(future::result(spawn_res).then(move |res| match res {
                Ok(()) => {
                    Box::new(client.send(OutputChunk::StartReadingStdin).map(|client| {
                        State::Executing(client, stdin_tx)
                    })) as LoopFuture<_>
                }
                Err(e) => {
                    Box::new(
                        client
                            .send(OutputChunk::Stderr(
                                format!("Failed to launch child `{}`: {:?}\n", cmd_desc, e)
                                    .into(),
                            ))
                            .and_then(move |client| client.send(OutputChunk::Exit(1)))
                            .map(|_| {
                                // Drop the client and exit.
                                State::Exited(1)
                            }),
                    ) as LoopFuture<_>
                }
            }))
        }
        (State::Executing(client, child), Event::Client(InputChunk::Stdin(bytes))) => {
            let noisy_stdin = config.noisy_stdin;
            Box::new(
                child
                    .send(ChildInput::Stdin(bytes))
                    .map_err(send_to_io)
                    .and_then(move |child| {
                        // If noisy_stdin is configured, respond with `StartReadingStdin`.
                        let respond = if noisy_stdin {
                            Box::new(client.send(OutputChunk::StartReadingStdin)) as IOFuture<C>
                        } else {
                            Box::new(future::ok::<_, io::Error>(client)) as IOFuture<C>
                        };
                        respond.map(|client| State::Executing(client, child))
                    }),
            ) as LoopFuture<_>
        }
        (State::Executing(client, child), Event::Client(InputChunk::StdinEOF)) => {
            Box::new(child.send(ChildInput::StdinEOF).map_err(send_to_io).map(
                |child| State::Executing(client, child),
            )) as LoopFuture<_>
        }
        (State::Executing(client, child), Event::Process(child_output)) => {
            let exit_code = match child_output {
                ChildOutput::Exit(code) => Some(code),
                _ => None,
            };
            Box::new(client.send(child_output.into()).map(move |client| {
                if let Some(code) = exit_code {
                    State::Exited(code)
                } else {
                    State::Executing(client, child)
                }
            })) as LoopFuture<_>
        }
        (s, Event::Client(InputChunk::Heartbeat)) => {
            // Not documented in the spec, but presumably always valid and ignored?
            ok(s)
        }
        (s, e) => {
            Box::new(future::err(
                err(&format!("Invalid event {:?} during phase {:?}", e, s)),
            ))
        }
    }
}

fn ok<T: 'static>(t: T) -> IOFuture<T> {
    Box::new(future::ok(t))
}

pub fn err(e: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

impl From<ChildOutput> for OutputChunk {
    fn from(co: ChildOutput) -> Self {
        match co {
            ChildOutput::Stdout(bytes) => OutputChunk::Stdout(bytes),
            ChildOutput::Stderr(bytes) => OutputChunk::Stderr(bytes),
            ChildOutput::Exit(code) => OutputChunk::Exit(code),
        }
    }
}

type LoopFuture<C> = IOFuture<State<C>>;

type IOFuture<T> = Box<Future<Item = T, Error = io::Error>>;

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
 #[cfg_attr(rustfmt, rustfmt_skip)]
trait ClientSink: Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static {}
 #[cfg_attr(rustfmt, rustfmt_skip)]
impl<T> ClientSink for T where T: Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static {}
