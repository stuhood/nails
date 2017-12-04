use std::str;

use futures::{Future, Stream};
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;

use codec::Codec;
use proto::Proto;

pub fn serve(addr: &str) {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = addr.parse().unwrap();

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(move |(socket, _)| {
        println!("Got connection: {:?}", socket);
        let transport = socket.framed(Codec);

        handle.spawn(Proto::execute(transport).map_err(|_| ()));

        Ok(())
    });

    core.run(server).unwrap();
}
