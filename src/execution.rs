use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{self, Command, Stdio};
use std::fmt::Debug;

use bytes::Bytes;
use futures_cpupool::CpuPool;
use futures::{stream, Future, Stream, Sink};
use futures::sync::mpsc;
use tokio_core::reactor::Handle;

lazy_static! {
    // Each member of this pool will be blocked reading one of stderr/stdout/exit for
    // a child process.
    // TODO: Should really be unbounded, or not a pool.
    static ref POOL: CpuPool = CpuPool::new(16);
}

// Total memory usage should be BUF_SIZE * BUF_COUNT.
const BUF_SIZE: usize = 4096;
const BUF_COUNT: usize = 128;

#[derive(Debug, Default)]
pub struct Args {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct Child(String);

#[derive(Debug)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel() -> (mpsc::Sender<ChildOutput>, mpsc::Receiver<ChildOutput>) {
    mpsc::channel(BUF_COUNT)
}

pub fn spawn<P: ProcessSink>(
    cmd: String,
    args: Args,
    working_dir: PathBuf,
    sink: P,
    handle: &Handle,
) -> Result<Child, io::Error> {
    let mut child = Command::new(cmd)
        .args(args.args)
        .env_clear()
        .envs(args.env)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()?;
    let description = format!("{:?}", child);

    let stdout_stream =
        stream_for(child.stdout.take().unwrap()).map(|bytes| ChildOutput::Stdout(bytes.into()));
    let stderr_stream =
        stream_for(child.stderr.take().unwrap()).map(|bytes| ChildOutput::Stderr(bytes.into()));
    let exit_stream = stream_for_exit(child).map(|exit_status| {
        ChildOutput::Exit(exit_status.code().unwrap_or(1))
    });

    // Fully consume the stdout/stderr streams before waiting on the exit stream.
    let stream = stdout_stream
        .select(stderr_stream)
        .chain(exit_stream);

    handle.spawn(
        sink.sink_map_err(|e| {
            io::Error::new(io::ErrorKind::Other, format!("Failed to send: {:?}", e))
        }).send_all(stream)
            .then(|_| Ok(())),
    );
    Ok(Child(description))
}

///
/// A stream to read the given Read instance to completion.
///
fn stream_for<R: Read + Send + Sized + 'static>(
    r: R,
) -> Box<Stream<Item = Vec<u8>, Error = io::Error>> {
    Box::new(stream::unfold(Some(r), |mut pipe_opt| {
        pipe_opt.take().map(|mut pipe| {
            POOL.spawn_fn(move || {
                let mut bytes = vec![0; BUF_SIZE];
                let read = pipe.read(&mut bytes)?;
                bytes.truncate(read);
                let next_state = if read == 0 { None } else { Some(pipe) };
                Ok((bytes, next_state))
            })
        })
    }))
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

///
///TODO: See https://users.rust-lang.org/t/why-cant-type-aliases-be-used-for-traits/10002/4
///
pub trait ProcessSink
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
