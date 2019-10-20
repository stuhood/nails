use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::sync::mpsc;
use futures::{future, Future, Sink, Stream};
use tokio_codec::Framed;
use tokio_core::reactor::Handle;
use tokio_io::{AsyncRead, AsyncWrite};

use codec::{InputChunk, OutputChunk, ServerCodec};
use execution::{child_channel, send_to_io, Args, ChildInput, ChildOutput, Command, Env, ExitCode};

#[derive(Debug)]
enum State<C: ClientSink> {
    // While we're gathering arguments and environment, but before the working directory has
    // arrived.
    Initializing(C, mpsc::Sender<ChildOutput>, Args, Env),
    // After the working directory has arrived, but before the command arrives.
    PreCommand(C, mpsc::Sender<ChildOutput>, Args, Env, PathBuf),
    // Executing, and able to receive stdin.
    Executing(C, mpsc::Sender<ChildInput>),
    // Process has finished executing.
    Exited(ExitCode),
}

#[derive(Debug)]
enum Event {
    Client(InputChunk),
    Process(ChildOutput),
}

pub fn execute<T, N>(
    handle: Handle,
    transport: Framed<T, ServerCodec>,
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
                State::Initializing(
                    client_write,
                    process_write,
                    Default::default(),
                    Default::default(),
                ),
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
        (State::Initializing(c, p, mut args, env), Event::Client(InputChunk::Argument(arg))) => {
            args.push(arg);
            ok(State::Initializing(c, p, args, env))
        }
        (
            State::Initializing(c, p, args, mut env),
            Event::Client(InputChunk::Environment { key, val }),
        ) => {
            env.push((key, val));
            ok(State::Initializing(c, p, args, env))
        }
        (
            State::Initializing(c, p, args, env),
            Event::Client(InputChunk::WorkingDir(working_dir)),
        ) => ok(State::PreCommand(c, p, args, env, working_dir)),
        (
            State::PreCommand(client, output_sink, args, env, working_dir),
            Event::Client(InputChunk::Command(command)),
        ) => {
            let cmd_desc = command.clone();
            let cmd = Command {
                command,
                args,
                env,
                working_dir,
            };
            let (stdin_tx, stdin_rx) = child_channel::<ChildInput>();
            let spawn_res = config.nail.spawn(cmd, output_sink, stdin_rx, handle);
            Box::new(future::result(spawn_res).then(move |res| {
                match res {
                    Ok(()) => Box::new(
                        client
                            .send(OutputChunk::StartReadingStdin)
                            .map(|client| State::Executing(client, stdin_tx)),
                    ) as LoopFuture<_>,
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
                                    State::Exited(ExitCode(1))
                                }),
                        ) as LoopFuture<_>
                    }
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
        (State::Executing(client, child), Event::Client(InputChunk::StdinEOF)) => Box::new(
            child
                .send(ChildInput::StdinEOF)
                .map_err(send_to_io)
                .map(|child| State::Executing(client, child)),
        )
            as LoopFuture<_>,
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
        (s, e) => Box::new(future::err(err(&format!(
            "Invalid event {:?} during phase {:?}",
            e, s
        )))),
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
            ChildOutput::Exit(code) => OutputChunk::Exit(code.0),
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
