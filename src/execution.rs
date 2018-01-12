use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::fmt::Debug;

use bytes::{BufMut, Bytes, BytesMut};
use futures_cpupool::{CpuFuture, CpuPool};
use futures::{stream, Future, Stream, Sink};
use futures::sync::mpsc;
use tokio_core::reactor::Handle;

lazy_static! {
    // Each member of this pool will be blocked reading one of stderr/stdout/exit for
    // a child process.
    // TODO: Should really be unbounded, or not a pool.
    static ref POOL: CpuPool = CpuPool::new(16);
}

// Total memory usage per channel should be BUF_SIZE * BUF_COUNT.
const BUF_SIZE: usize = 4096;
const BUF_COUNT: usize = 128;
const BUF_TOTAL: usize = BUF_SIZE * BUF_COUNT;

#[derive(Debug, Default)]
pub struct Args {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum ChildInput {
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(BUF_COUNT)
}

pub fn send_to_io<T: Debug>(e: mpsc::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("Failed to send: {:?}", e))
}

fn unreachable_io() -> io::Error {
    unreachable!()
}

pub fn spawn(
    cmd: String,
    args: Args,
    working_dir: PathBuf,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
    handle: &Handle,
) -> Result<(), io::Error> {
    let mut child = Command::new(cmd.clone())
        .args(args.args)
        .env_clear()
        .envs(args.env)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()?;


    // Copy inputs to the child.
    handle.spawn(
        sink_for(child.stdin.take().unwrap(), handle)
            .send_all(
                input_stream
                    .map(|child_input| match child_input {
                        ChildInput::Stdin(bytes) => bytes,
                        ChildInput::StdinEOF => Bytes::new(),
                    })
                    .map_err(|_| unreachable_io()),
            )
            .then(|_| Ok(())),
    );

    // Fully consume the stdout/stderr streams before waiting on the exit stream.
    let stdout_stream =
        stream_for(child.stdout.take().unwrap()).map(|bytes| ChildOutput::Stdout(bytes.into()));
    let stderr_stream =
        stream_for(child.stderr.take().unwrap()).map(|bytes| ChildOutput::Stderr(bytes.into()));
    let exit_stream = stream_for_exit(child).map(|exit_status| {
        ChildOutput::Exit(exit_status.code().unwrap_or_else(
            || panic!("{:?}", exit_status),
        ))
    });
    let output_stream = stdout_stream.select(stderr_stream).chain(exit_stream);

    // Spawn a task to send all of stdout/sterr/exit to our output sink.
    handle.spawn(
        output_sink
            .sink_map_err(send_to_io)
            .send_all(output_stream)
            .then(|_| Ok(())),
    );

    Ok(())
}

///
/// A stream to read the given Read instance to completion. The last item on the stream
/// will be an empty Bytes instance.
///
fn stream_for<R: Read + Send + Sized + 'static>(
    r: R,
) -> Box<Stream<Item = Bytes, Error = io::Error>> {
    Box::new(stream::unfold(Some((r, BytesMut::new())), |mut r_opt| {
        r_opt.take().map(|(mut r, mut buf)| {
            POOL.spawn_fn(move || {
                if buf.len() < BUF_SIZE {
                    // Allocate and zero a buffer which we will consume to give away.
                    buf = BytesMut::with_capacity(BUF_TOTAL);
                    buf.put_slice(&[0; BUF_TOTAL]);
                }
                let read = r.read(&mut buf)?;
                let remainder = buf.split_off(read);
                let next_state = if read == 0 {
                    None
                } else {
                    Some((r, remainder))
                };
                Ok((buf.freeze(), next_state))
            })
        })
    }))
}

///
/// A sink to write to the given Write instance. An empty Bytes instance triggers close.
///
fn sink_for<W: Write + Send + Sized + 'static>(
    w: W,
    handle: &Handle,
) -> Box<Sink<SinkItem = Bytes, SinkError = io::Error>> {
    let (sink, stream) = mpsc::channel::<Bytes>(BUF_COUNT);
    handle.spawn(
        stream
            .map_err(|_| unreachable_io())
            .fold(Some(w), |w, bytes| {
                POOL.spawn_fn(move || match (w, bytes.len()) {
                    (_, 0) => {
                        println!("Got empty input: dropping stdin.");
                        Ok(None)
                    }
                    (Some(mut w), _) => {
                        w.write_all(&bytes)?;
                        Ok(Some(w))
                    }
                    (None, _) => {
                        // TODO: This should probably be a protocol state.
                        Err(io::Error::new(
                            io::ErrorKind::Other,
                            "stdin is already closed.",
                        ))
                    }
                }) as CpuFuture<_, io::Error>
            })
            .then(|_| Ok(())),
    );
    Box::new(sink.sink_map_err(send_to_io))
}

///
/// A single item stream for the child's exit code.
///
fn stream_for_exit(
    child: process::Child,
) -> Box<Stream<Item = process::ExitStatus, Error = io::Error>> {
    Box::new(stream::unfold(Some(child), |mut child_opt| {
        child_opt.take().map(|mut child| {
            POOL.spawn_fn(move || child.wait().map(|exit_status| (exit_status, None)))
        })
    }))
}
