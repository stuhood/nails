extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;

pub mod execution;
mod codec;
mod proto;

use std::io;
use std::path::PathBuf;
use futures::Future;
use futures::sync::mpsc;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_io::AsyncRead;

use codec::Codec;
use execution::{Args, ChildInput, ChildOutput};

#[derive(Clone)]
pub struct Config<N: Nail> {
    nail: N,
    noisy_stdin: bool,
}

impl<N> Config<N>
where
    N: Nail,
{
    pub fn new(nail: N) -> Config<N> {
        Config {
            nail,
            noisy_stdin: true,
        }
    }

    /// Although it is not part of the spec, the Python and C clients require that
    /// `StartReadingStdin` is sent after every stdin chunk has been consumed.
    ///   see https://github.com/facebook/nailgun/issues/88
    pub fn noisy_stdin(mut self, value: bool) -> Self {
        self.noisy_stdin = value;
        self
    }
}

pub trait Nail: Clone + Send + Sync + 'static {
    fn spawn(
        &self,
        cmd: String,
        args: Args,
        working_dir: PathBuf,
        output_sink: mpsc::Sender<ChildOutput>,
        input_stream: mpsc::Receiver<ChildInput>,
        handle: &Handle,
    ) -> Result<(), io::Error>;
}

pub fn handle_connection<N: Nail>(
    config: Config<N>,
    handle: &Handle,
    socket: TcpStream,
) -> Result<(), io::Error> {
    socket.set_nodelay(true)?;

    handle.spawn(
        proto::execute(handle.clone(), socket.framed(Codec), config).map_err(|_| ()),
    );

    Ok(())
}
