extern crate bytes;
extern crate futures;
extern crate futures_cpupool;
#[macro_use]
extern crate lazy_static;
extern crate tokio_io;
extern crate tokio_core;

mod execution;
mod codec;
mod proto;

use futures::{Future, Stream};
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;

use codec::Codec;

fn main() {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = "127.0.0.1:2113".parse().unwrap();

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(move |(socket, _)| {
        println!("Got connection: {:?}", socket);
        let transport = socket.framed(Codec);

        handle.spawn(proto::execute(transport).map_err(|_| ()));

        Ok(())
    });

    core.run(server).unwrap();
}
