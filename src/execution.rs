use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::fmt;

use futures_cpupool::CpuPool;
use futures::{stream, Future, Stream};

lazy_static! {
    // TODO: Should probably be an unbounded pool.
    static ref POOL: CpuPool = CpuPool::new(16);
}

const READ_BUF_SIZE: usize = 1024;

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

pub enum ChildOutput {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(u8),
}

pub fn spawn(cmd: String, args: Args, working_dir: PathBuf, outputs: Sender<ChildOutput>) -> Result<Child, io::Error> {
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
            let mut bytes = vec![0; READ_BUF_SIZE];
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
