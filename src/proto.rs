use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::{future, Future, Stream, Sink};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::Framed;

use codec::{Codec, InputChunk, OutputChunk};
use execution::{self, Args, Child};

#[derive(Debug)]
enum State<C>
where
    C: Debug,
{
    // While we're gathering arguments and environment, but before the working directory has
    // arrived.
    Initializing(C, Args),
    // After the working directory has arrived, but before the command arrives.
    PreCommand {
        client: C,
        args: Args,
        working_dir: PathBuf,
    },
    // Executing, and able to receive stdin.
    Executing(C, Child),
    // Executing, after stdin has been closed.
    ExecutingPostStdin(C, Child),
    // Process has finished executing.
    Exited,
}

pub fn execute<T>(transport: Framed<T, Codec>) -> Box<Future<Item = (), Error = io::Error>>
where
    T: AsyncRead + AsyncWrite + Debug + 'static,
{
    let (write, read) = transport.split();
    Box::new(
        read.fold(State::Initializing(write, Default::default()), step)
            .then(|e| {
                println!("Connection closed in state {:?}", e);
                Ok(())
            }),
    )
}

fn step<C>(state: State<C>, chunk: InputChunk) -> Box<Future<Item = State<C>, Error = io::Error>>
where
    C: Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static,
{
    match (state, chunk) {
        (State::Initializing(client, mut args), InputChunk::Argument(arg)) => {
            args.args.push(arg);
            ok(State::Initializing(client, args))
        }
        (State::Initializing(client, mut args), InputChunk::Environment { key, val }) => {
            args.env.push((key, val));
            ok(State::Initializing(client, args))
        }
        (State::Initializing(client, args), InputChunk::WorkingDir(working_dir)) => {
            ok(State::PreCommand {
                client,
                args,
                working_dir,
            })
        }
        (State::PreCommand {
             client,
             args,
             working_dir,
         },
         InputChunk::Command(cmd)) => spawn(client, args, working_dir, cmd),
        (e @ State::Executing(..), InputChunk::Stdin(bytes)) => {
            // TODO: Send stdin to process.
            println!("Got stdin chunk: {:?}", bytes);
            ok(e)
        }
        (State::Executing(client, p), InputChunk::StdinEOF) => {
            // TODO: Enqueue stdin close.
            ok(State::ExecutingPostStdin(client, p))
        }
        (s, InputChunk::Heartbeat) => {
            // Not documented in the spec, but presumably always valid and ignored?
            ok(s)
        }
        (s, c) => {
            err(&format!(
                "Received invalid chunk {:?} during phase {:?}",
                c,
                s
            ))
        }
    }
}

fn spawn<C>(
    client: C,
    args: Args,
    working_dir: PathBuf,
    cmd: String,
) -> Box<Future<Item = State<C>, Error = io::Error>>
where
    C: Debug + Sink<SinkItem = OutputChunk, SinkError = io::Error> + 'static,
{
    Box::new(
        future::result(execution::spawn(cmd, args, working_dir)).then(move |res| match res {
            Ok(child) => {
                println!("Launched child as: {:?}", child);
                Box::new(client.send(OutputChunk::StartReadingStdin).map(|client| {
                    State::Executing(client, child)
                })) as Box<Future<Item = State<_>, Error = io::Error>>
            }
            Err(e) => {
                // TODO: Send as stderr.
                println!("Failed to launch child: {:?}", e);
                Box::new(client.send(OutputChunk::Exit(1)).map(|_| State::Exited)) as
                    Box<Future<Item = State<_>, Error = io::Error>>
            }
        }),
    )
}

fn ok<T: 'static>(t: T) -> Box<Future<Item = T, Error = io::Error>> {
    Box::new(future::ok(t))
}

pub fn err<T: 'static>(e: &str) -> Box<Future<Item = T, Error = io::Error>> {
    Box::new(future::err(io::Error::new(io::ErrorKind::Other, e)))
}
