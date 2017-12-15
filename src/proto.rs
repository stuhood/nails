use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::{future, Future, Stream, Sink};
use futures::sync::mpsc;
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::Framed;

use codec::{Codec, InputChunk, OutputChunk};
use execution::{self, Args, Child, ChildOutput, child_channel};

#[derive(Debug)]
enum State<C: ClientSink, P: ProcessSink> {
    // While we're gathering arguments and environment, but before the working directory has
    // arrived.
    Initializing(C, P, Args),
    // After the working directory has arrived, but before the command arrives.
    PreCommand(C, P, Args, PathBuf),
    // Executing, and able to receive stdin.
    Executing(C, Child),
    // Process has finished executing.
    Exited,
}

#[derive(Debug)]
enum Event {
    Client(InputChunk),
    Process(ChildOutput),
}

pub fn execute<T>(transport: Framed<T, Codec>) -> Box<Future<Item = (), Error = io::Error>>
where
    T: AsyncRead + AsyncWrite + Debug + 'static,
{
    // Create a channel to consume process output from a forked subprocess, and split the client
    // transport into write and read portions.
    let (process_write, process_read) = child_channel();
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
                step,
            )
            .then(|e| {
                println!("Connection finished in state {:?}", e);
                Ok(())
            }),
    )
}

fn step<C, P>(state: State<C, P>, ev: Event) -> Box<Future<Item = State<C, P>, Error = io::Error>>
where
    C: ClientSink,
    P: ProcessSink,
{
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
        (State::PreCommand(client, process, args, working_dir),
         Event::Client(InputChunk::Command(cmd))) => {
            Box::new(
                future::result(execution::spawn(cmd, args, working_dir)).then(move |res| match res {
                    Ok(child) => {
                        println!("Launched child as: {:?}", child);
                        Box::new(client.send(OutputChunk::StartReadingStdin).map(|client| {
                            State::Executing(client, child)
                        })) as LoopBox<_, _>
                    }
                    Err(e) => {
                        // TODO: Send as stderr.
                        println!("Failed to launch child: {:?}", e);
                        Box::new(client.send(OutputChunk::Exit(1)).map(|_| State::Exited)) as
                            LoopBox<_, _>
                    }
                }),
            )
        }
        (e @ State::Executing(..), Event::Client(InputChunk::Stdin(bytes))) => {
            // TODO: Send stdin to process.
            println!("Got stdin chunk: {:?}", bytes);
            ok(e)
        }
        (e @ State::Executing(..), Event::Client(InputChunk::StdinEOF)) => {
            // TODO: Enqueue stdin close to the child.
            println!("Got stdin eof.");
            ok(e)
        }
        (State::Executing(client, child), Event::Process(child_output)) => {
            let exit = match child_output {
                ChildOutput::Exit(_) => true,
                _ => false,
            };
            Box::new(client.send(child_output.into()).map(
                move |client| if exit {
                    State::Exited
                } else {
                    State::Executing(client, child)
                },
            )) as LoopBox<_, _>
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

fn ok<T: 'static>(t: T) -> Box<Future<Item = T, Error = io::Error>> {
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

type LoopBox<C, P> = Box<Future<Item = State<C, P>, Error = io::Error>>;

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
trait ClientSink
    : Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static {
}
impl<T> ClientSink for T
where
    T: Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static,
{
}

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
trait ProcessSink
    : Debug + Sink<SinkItem = ChildOutput, SinkError = mpsc::SendError<ChildOutput>> + 'static
    {
}
impl<T> ProcessSink for T
where
    T: Debug
        + Sink<SinkItem = ChildOutput, SinkError = mpsc::SendError<ChildOutput>>
        + 'static,
{
}
