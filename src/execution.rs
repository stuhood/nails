use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::fmt;

use bytes::Bytes;
use futures_cpupool::CpuPool;
use futures::{stream, Future, Stream};
use futures::sync::mpsc;

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

pub struct Child {
    description: String,
    stdout: Box<Stream<Item = Option<Vec<u8>>, Error = io::Error>>,
    stderr: Box<Stream<Item = Option<Vec<u8>>, Error = io::Error>>,
    exit: Box<Future<Item = ExitStatus, Error = io::Error>>,
}

impl fmt::Debug for Child {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Child")
            .field("description", &self.description)
            .finish()
    }
}

#[derive(Debug)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(u8),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel() -> (mpsc::Sender<ChildOutput>, mpsc::Receiver<ChildOutput>) {
    mpsc::channel(BUF_COUNT)
}

pub fn spawn(cmd: String, args: Args, working_dir: PathBuf) -> Result<Child, io::Error> {
    Command::new(cmd)
        .args(args.args)
        .env_clear()
        .envs(args.env)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped())
        .spawn()
        .map(|mut child| {
            let description = format!("{:?}", child);
            let stdout = stream_for(child.stdout.take().unwrap());
            let stderr = stream_for(child.stderr.take().unwrap());
            let exit = Box::new(POOL.spawn_fn(move || child.wait()));
            Child {
                description,
                stdout,
                stderr,
                exit,
            }
        })
}

///
/// TODO: The `Option` in the stream is because we need an unfold that operates on Future<Option>
/// rather than Option<Future>.
///
fn stream_for<R: Read + Send + Sized + 'static>(
    r: R,
) -> Box<Stream<Item = Option<Vec<u8>>, Error = io::Error>> {
    Box::new(stream::unfold(r, |mut pipe| {
        Some(POOL.spawn_fn(move || {
            let mut bytes = vec![0; BUF_SIZE];
            match pipe.read(&mut bytes)? {
                0 => Ok((None, pipe)),
                read => {
                    bytes.truncate(read);
                    Ok((Some(bytes), pipe))
                }
            }
        }))
    }))
}
