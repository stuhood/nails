use std::default::Default;
use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use futures::channel::mpsc;
use futures::{stream, Sink, SinkExt, StreamExt, TryFutureExt, TryStreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{InputChunk, OutputChunk, ServerCodec};
use crate::execution::{
    child_channel, send_to_io, Args, ChildInput, ChildOutput, Command, Env, ExitCode,
};

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

pub async fn execute<R, W, N>(read: R, write: W, config: super::Config<N>) -> Result<(), io::Error>
where
    R: AsyncRead + Debug + Unpin + Send,
    W: AsyncWrite + Debug + Unpin + Send,
    N: super::Nail,
{
    // Create a channel to consume process output from a forked subprocess, and split the client
    // transport into write and read portions.
    let (process_write, process_read) = child_channel::<ChildOutput>();
    let client_read = FramedRead::new(read, ServerCodec);
    let client_write = FramedWrite::new(write, ServerCodec);

    // Select on the two input sources to create a merged Stream of events.
    let mut events_read = stream::select(
        process_read.map(|v| Ok(Event::Process(v))),
        client_read.map_ok(|e| Event::Client(e)),
    );

    // Loop consuming the merged Stream until it errors, or until consumption of it does.
    let mut state = State::Initializing(
        client_write,
        process_write,
        Default::default(),
        Default::default(),
    );
    while let Some(event) = events_read.next().await {
        state = step(&config, state, event?).await?;
    }
    Ok(())
}

async fn step<C: ClientSink, N: super::Nail>(
    config: &super::Config<N>,
    state: State<C>,
    ev: Event,
) -> Result<State<C>, io::Error> {
    match (state, ev) {
        (State::Initializing(c, p, mut args, env), Event::Client(InputChunk::Argument(arg))) => {
            args.push(arg);
            Ok(State::Initializing(c, p, args, env))
        }
        (
            State::Initializing(c, p, args, mut env),
            Event::Client(InputChunk::Environment { key, val }),
        ) => {
            env.push((key, val));
            Ok(State::Initializing(c, p, args, env))
        }
        (
            State::Initializing(c, p, args, env),
            Event::Client(InputChunk::WorkingDir(working_dir)),
        ) => Ok(State::PreCommand(c, p, args, env, working_dir)),
        (
            State::PreCommand(mut client, output_sink, args, env, working_dir),
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
            match config.nail.spawn(cmd, output_sink, stdin_rx) {
                Ok(()) => {
                    client.send(OutputChunk::StartReadingStdin).await?;
                    Ok(State::Executing(client, stdin_tx))
                }
                Err(e) => {
                    let e = format!("Failed to launch child `{}`: {:?}\n", cmd_desc, e);
                    client.send(OutputChunk::Stderr(e.into())).await?;
                    client.send(OutputChunk::Exit(1)).await?;
                    // Drop the client and exit.
                    Ok(State::Exited(ExitCode(1)))
                }
            }
        }
        (State::Executing(mut client, mut child), Event::Client(InputChunk::Stdin(bytes))) => {
            let noisy_stdin = config.noisy_stdin;
            child
                .send(ChildInput::Stdin(bytes))
                .map_err(send_to_io)
                .await?;
            // If noisy_stdin is configured, respond with `StartReadingStdin`.
            if noisy_stdin {
                client.send(OutputChunk::StartReadingStdin).await?;
            }
            Ok(State::Executing(client, child))
        }
        (State::Executing(client, mut child), Event::Client(InputChunk::StdinEOF)) => {
            child.send(ChildInput::StdinEOF).map_err(send_to_io).await?;
            Ok(State::Executing(client, child))
        }
        (State::Executing(mut client, child), Event::Process(child_output)) => {
            let exit_code = match child_output {
                ChildOutput::Exit(code) => Some(code),
                _ => None,
            };
            client.send(child_output.into()).await?;
            if let Some(code) = exit_code {
                Ok(State::Exited(code))
            } else {
                Ok(State::Executing(client, child))
            }
        }
        (s, Event::Client(InputChunk::Heartbeat)) => {
            // Not documented in the spec, but presumably always valid and ignored?
            Ok(s)
        }
        (s, e) => {
            let e = format!("Invalid event {:?} during phase {:?}", e, s);
            Err(err(&e))
        }
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
