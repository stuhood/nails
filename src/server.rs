use std::str;

use futures::Stream;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;

use codec::Codec;

fn serve(addr: &str) {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = addr.parse().unwrap();

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    let server = listener.incoming().for_each(move |(socket, _)| {
        let transport = socket.framed(Codec);

        let process_connection = transport.fold(|chunk| {
            // Consume initialization chunks, then send accept for stdin.
            ...
        });

        handle.spawn(process_connection.map_err(|_| ()));

        Ok(())
    });

    core.run(server).unwrap();
}
