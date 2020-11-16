use std::time::Duration;

use futures::StreamExt;
use tokio::net::TcpListener;

use nails_fork::ForkNail;

#[tokio::main]
async fn main() {
    env_logger::init();

    let config = nails::Config::default().heartbeat_frequency(Duration::from_millis(500));
    let nail = ForkNail;

    let mut listener = TcpListener::bind("127.0.0.1:2113").await.unwrap();
    println!("Bound listener: {:?}", listener);

    while let Some(socket) = listener.incoming().next().await {
        println!("Got connection: {:?}", socket);
        tokio::spawn(nails::server::handle_connection(
            config.clone(),
            nail.clone(),
            socket.unwrap(),
        ));
    }
}
