use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus};

use futures_cpupool::CpuPool;
use futures::{future, stream, Future, Stream};

lazy_static! {
    // TODO: Should probably be an unbounded pool.
    static ref POOL: CpuPool = CpuPool::new(16);
}

const READ_BUF_SIZE: usize = 1024;

pub struct Child {
    stdout: Box<Stream<Item=Option<Vec<u8>>, Error=io::Error>>,
    stderr: Box<Stream<Item=Option<Vec<u8>>, Error=io::Error>>,
    exit: Box<Future<Item=ExitStatus, Error=io::Error>>,
}

pub fn spawn(
    cmd: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    working_dir: PathBuf,
) -> Box<Future<Item=Child, Error=io::Error>> {
    let child =
        Command::new(cmd)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(working_dir)
            .spawn();
    let res = future::result(child)
        .and_then(|mut child| {
            let stdout = stream_for(child.stdout.take().unwrap());
            let stderr = stream_for(child.stderr.take().unwrap());
            let exit = Box::new(POOL.spawn_fn(move || child.wait()));
            Ok(Child { stdout, stderr, exit })
        });
    Box::new(res)
}

///
/// TODO: The `Option` in the stream is because we Need an unfold that operates on Future<Option>
/// rather than Option<Future>.
///
fn stream_for<R: Read + Send + Sized + 'static>(r: R) -> Box<Stream<Item=Option<Vec<u8>>, Error=io::Error>> {
    Box::new(
        stream::unfold(r, |mut pipe| {
            Some(
                POOL.spawn_fn(move || {
                    let mut bytes = vec![0; READ_BUF_SIZE];;
                    match pipe.read(&mut bytes)? {
                        0 =>
                            Ok((None, pipe)),
                        read => {
                            bytes.truncate(read);
                            Ok((Some(bytes), pipe))
                        },
                    }
                })
            )
        })
    )
}
