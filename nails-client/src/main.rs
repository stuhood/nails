use nails;

use env_logger;

use std::env;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures::channel::mpsc;
use futures::{future, SinkExt, Stream, StreamExt, TryFutureExt};
use log::debug;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use nails::execution::{child_channel, send_to_io, stream_for, ChildInput, ChildOutput, Command};

///
/// Split env::args into a nailgun server address, and the args to send to the server.
///
fn parse_args(
    mut args: impl Iterator<Item = String>,
) -> Result<(SocketAddr, String, Vec<String>), String> {
    // Discard the process process name.
    args.next();

    let remote_addr = if let Some(host_and_port) = args.next() {
        host_and_port
            .parse()
            .map_err(|e| format!("Failed to parse {}: {}", host_and_port, e))?
    } else {
        return Err(
            "Expected a leading argument representing a nailgun server hostname and port."
                .to_owned(),
        );
    };

    if let Some(command) = args.next() {
        Ok((remote_addr, command, args.collect()))
    } else {
        Err("Needed at least one trailing argument for the command to run.".to_owned())
    }
}

async fn handle_stdio(
    mut stdio_read: impl Stream<Item = ChildOutput> + Unpin,
) -> Result<(), io::Error> {
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    while let Some(output) = stdio_read.next().await {
        match output {
            ChildOutput::Stdout(bytes) => stdout.write_all(&bytes).await?,
            ChildOutput::Stderr(bytes) => stderr.write_all(&bytes).await?,
        }
    }
    Ok(())
}

async fn handle_stdin(mut stdin_write: mpsc::Sender<ChildInput>) -> Result<(), io::Error> {
    let mut stdin = stream_for(tokio::io::stdin());
    while let Some(input_bytes) = stdin.next().await {
        stdin_write
            .send(ChildInput::Stdin(input_bytes?))
            .await
            .map_err(send_to_io)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), String> {
    env_logger::init();
    let (addr, command, args) = parse_args(env::args())?;

    let working_dir = env::current_dir()
        .map_err(|e| format!("Could not detect current working directory: {}", e))?;

    let cmd = Command {
        command,
        args,
        env: env::vars().collect(),
        working_dir,
    };

    // TODO: This aligns with the C client. Possible that the default client and server configs
    // should be different in order to be maximally lenient.
    let config = nails::Config::default().heartbeat_frequency(Duration::from_millis(500));

    // And handle the connection in the foreground.
    debug!("Connecting to server at {}...", addr);
    let stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| format!("Error connecting to server: {}", e))?;
    let mut child = nails::client::handle_connection(config, stream, cmd, async {
        let (stdin_write, stdin_read) = child_channel::<ChildInput>();
        let _join = tokio::spawn(handle_stdin(stdin_write));
        stdin_read
    })
    .map_err(|e| format!("Error starting process: {}", e))
    .await?;

    // Spawn a task to copy stdout/stderr to stdio.
    let output_stream = child.output_stream.take().unwrap();
    let stdio_printer = async move { tokio::spawn(handle_stdio(output_stream)).await.unwrap() };

    let (_, exit_code) = future::try_join(stdio_printer, child.wait())
        .await
        .map_err(|e| format!("Error executing process: {}", e))?;

    debug!("Exiting with {}", exit_code.0);
    std::process::exit(exit_code.0);
}
