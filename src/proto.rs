use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::{future, Future, Stream, Sink};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_io::codec::Framed;

use codec::{Codec, InputChunk, OutputChunk};

#[derive(Debug, Default)]
pub struct Args {
    args: Vec<String>,
    env: Vec<(String, String)>,
}

type Proc = String;

#[derive(Debug)]
enum State<T> where T: Debug {
    // While we're gathering arguments and environment, but before the working directory has
    // arrived.
    Initializing(T, Args),
    // After the working directory has arrived, but before the command arrives.
    PreCommand {
        client: T,
        args: Args,
        working_dir: PathBuf,
    },
    // Executing, and able to receive stdin.
    Executing(T, Proc),
    // Executing, after stdin has been closed.
    ExecutingPostStdin(T, Proc),
    // Process has finished executing.
    Exited,
}

pub struct Proto;

impl Proto {
    pub fn execute<T>(transport: Framed<T, Codec>) -> Box<Future<Item=(), Error=io::Error>>
        where T: AsyncRead + AsyncWrite + Debug + 'static
    {
        let (write, read) = transport.split();
        Box::new(
            read.fold(State::Initializing(write, Default::default()), move |state, chunk| match (state, chunk) {
                (State::Initializing(client, mut args), InputChunk::Argument(arg)) => {
                    args.args.push(arg);
                    ok(State::Initializing(client, args))
                },
                (State::Initializing(client, mut args), InputChunk::Environment { key, val }) => {
                    args.env.push((key, val));
                    ok(State::Initializing(client, args))
                },
                (State::Initializing(client, args), InputChunk::WorkingDir(working_dir)) => {
                    ok(State::PreCommand { client, args, working_dir })
                },
                (State::PreCommand { client, args, working_dir }, InputChunk::Command(cmd)) => {
                    // TODO: Start process, signal stdin.
                    let p = format!("Executing {:?} {:?} {:?}", args, working_dir, cmd);
                    println!("{}", p);
                    Box::new(
                        client
                            .send(OutputChunk::StartReadingStdin)
                            .and_then(|client| Ok(State::Executing(client, p)))
                    )
                },
                (e @ State::Executing(..), InputChunk::Stdin(bytes)) => {
                    // TODO: Send stdin to process.
                    println!("Got stdin chunk: {:?}", bytes);
                    ok(e)
                },
                (State::Executing(client, p), InputChunk::StdinEOF) => {
                    // TODO: Enqueue stdin close.
                    ok(State::ExecutingPostStdin(client, p))
                },
                (s, c) => {
                    err(&format!("Received invalid chunk {:?} during phase {:?}", c, s))
                },
            })
            .and_then(|_| Ok(()))
        )
    }
}

fn ok<T: 'static>(t: T) -> Box<Future<Item=T, Error=io::Error>> {
    Box::new(future::ok(t))
}

pub fn err<T: 'static>(e: &str) -> Box<Future<Item=T, Error=io::Error>> {
    Box::new(future::err(io::Error::new(io::ErrorKind::Other, e)))
}
