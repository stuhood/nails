use std::time::Duration;

use tokio::net::TcpListener;

use nails_fork::ForkNail;

#[tokio::main]
async fn main() {
    env_logger::init();

    let config = nails::Config::default().heartbeat_frequency(Duration::from_millis(500));
    let nail = ForkNail;

    let listener = TcpListener::bind("127.0.0.1:2113").await.unwrap();
    println!("Bound listener: {:?}", listener);

    while let Ok((socket, _addr)) = listener.accept().await {
        println!("Got connection: {:?}", socket);
        tokio::spawn(nails::server::handle_connection(
            config.clone(),
            nail.clone(),
            socket,
        ));
    }
}
