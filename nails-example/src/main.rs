extern crate nails;
extern crate nails_fork;

extern crate futures;
extern crate tokio_core;

use futures::Stream;
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;

use nails_fork::ForkNail;

fn main() {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = "127.0.0.1:2113".parse().unwrap();

    let config = nails::Config::new(ForkNail);

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(|(socket, _)| {
        println!("Got connection: {:?}", socket);
        nails::handle_connection(config.clone(), &handle, socket)
    });

    core.run(server).unwrap();
}
