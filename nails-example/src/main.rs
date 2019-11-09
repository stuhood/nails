use nails;

use futures::TryStreamExt;
use tokio::net::TcpListener;
use tokio::runtime::Runtime;

use nails_fork::ForkNail;

fn main() {
    let mut runtime = Runtime::new().unwrap();

    let config = nails::Config::new(ForkNail);

    let listener = runtime
        .block_on(TcpListener::bind("127.0.0.1:2113"))
        .unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().try_for_each(move |socket| {
        println!("Got connection: {:?}", socket);
        nails::server_handle_connection(config.clone(), socket)
    });

    runtime.block_on(server).unwrap();
}
