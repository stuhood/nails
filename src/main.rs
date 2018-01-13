extern crate bytes;
extern crate futures;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_process;

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
        socket.set_nodelay(true)?;

        let transport = socket.framed(Codec);
        let config = proto::Config { noisy_stdin: true };

        handle.spawn(proto::execute(handle.clone(), transport, config).map_err(
            |_| (),
        ));

        Ok(())
    });

    core.run(server).unwrap();
}
