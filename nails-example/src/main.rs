use nails;

use futures::Stream;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use nails_fork::ForkNail;

fn main() {
    let mut runtime = Runtime::new().unwrap();
    let remote_addr = "127.0.0.1:2113".parse().unwrap();

    let config = nails::Config::new(ForkNail);

    let listener = TcpListener::bind(&remote_addr).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(move |socket| {
        println!("Got connection: {:?}", socket);
        nails::server_handle_connection(config.clone(), socket)
    });

    runtime.block_on(server).unwrap();
}
