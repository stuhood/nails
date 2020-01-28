use nails;

use futures::StreamExt;
use tokio::net::TcpListener;

use nails_fork::ForkNail;

#[tokio::main]
async fn main() {
    let config = nails::Config::new(ForkNail);

    let mut listener = TcpListener::bind("127.0.0.1:2113").await.unwrap();
    println!("Bound listener: {:?}", listener);

    while let Some(socket) = listener.incoming().next().await {
        println!("Got connection: {:?}", socket);
        tokio::spawn(nails::server_handle_connection(
            config.clone(),
            socket.unwrap(),
        ));
    }
}
