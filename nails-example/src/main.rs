extern crate nails;

extern crate tokio_core;
extern crate futures;

use futures::Stream;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;


const FORK_CONFIG: nails::Config = nails::Config { noisy_stdin: true };

fn main() {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = "127.0.0.1:2113".parse().unwrap();

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(|(socket, _)| {
        println!("Got connection: {:?}", socket);
        nails::handle_connection(&FORK_CONFIG, &handle, socket)
    });

    core.run(server).unwrap();
}
