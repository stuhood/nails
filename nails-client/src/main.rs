use nails;

use env_logger;

use std::env;
use std::io;
use std::net::SocketAddr;

use futures::channel::mpsc;
use futures::{SinkExt, Stream, StreamExt, TryFutureExt};
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
            ChildOutput::Exit(_) => {
                // NB: We ignore exit here and allow the main thread to handle exiting.
                break;
            }
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
    stdin_write
        .send(ChildInput::StdinEOF)
        .await
        .map_err(send_to_io)?;
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

    // Spawn tasks to read stdout/stderr and write stdin.
    let (stdio_write, stdio_read) = child_channel::<ChildOutput>();
    let (stdin_write, stdin_read) = child_channel::<ChildInput>();
    let stdio_printer = tokio::spawn(handle_stdio(stdio_read));
    let _join = tokio::spawn(handle_stdin(stdin_write));

    // And handle the connection in the foreground.
    debug!("Connecting to server at {}...", addr);
    let exit_code = TcpStream::connect(&addr)
        .and_then(|stream| nails::client_handle_connection(stream, cmd, stdio_write, stdin_read))
        .map_err(|e| format!("Error communicating with server: {}", e))
        .await?;

    stdio_printer
        .await
        .unwrap()
        .map_err(|e| format!("Error flushing stdio: {}", e))?;

    debug!("Exiting with {}", exit_code.0);
    std::process::exit(exit_code.0);
}
