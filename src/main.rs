extern crate bytes;
extern crate futures;
extern crate tokio_io;
extern crate tokio_core;

mod codec;
mod server;

fn main() {
    server::serve("127.0.0.1:12000")
}
