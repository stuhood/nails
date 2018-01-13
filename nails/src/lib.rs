extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;

mod execution;
mod codec;
mod proto;

use std::io;
use futures::Future;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_io::AsyncRead;

use codec::Codec;

pub struct Config {
    /// Although it is no part of the spec, the Python and C clients require that
    /// `StartReadingStdin` is sent after every stdin chunk has been consumed.
    ///   see https://github.com/facebook/nailgun/issues/88
    pub noisy_stdin: bool,
}

pub fn handle_connection(
    config: &'static Config,
    handle: &Handle,
    socket: TcpStream,
) -> Result<(), io::Error> {
    println!("Got connection: {:?}", socket);
    socket.set_nodelay(true)?;

    handle.spawn(
        proto::execute(handle.clone(), socket.framed(Codec), config).map_err(|_| ()),
    );

    Ok(())
}
