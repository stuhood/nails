use nails;

use env_logger;

use std::env;
use std::io::{self, Write};
use std::net::SocketAddr;

use futures::{future, Stream, StreamExt, TryFutureExt};
use log::debug;
use tokio::net::TcpStream;
use tokio::runtime::Runtime;

use nails::execution::{child_channel, ChildInput, ChildOutput, Command};

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

async fn print_stdio(
    mut stdio_read: impl Stream<Item = ChildOutput> + Unpin,
) -> Result<(), io::Error> {
    while let Some(output) = stdio_read.next().await {
        match output {
            ChildOutput::Stdout(bytes) => io::stdout().write_all(&bytes)?,
            ChildOutput::Stderr(bytes) => io::stderr().write_all(&bytes)?,
            ChildOutput::Exit(_) => {
                // NB: We ignore exit here and allow the main thread to handle exiting.
                break;
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
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

    let mut runtime = Runtime::new().unwrap();

    // A task to render stdout.
    // TODO: This is blocking, and should probably use tokio's stdio facilities instead.
    let (stdio_write, stdio_read) = child_channel::<ChildOutput>();
    let stdio_printer = print_stdio(stdio_read);

    // And the connection.
    // TODO: Send stdin.
    let (_stdin_write, stdin_read) = child_channel::<ChildInput>();
    let connection = TcpStream::connect(&addr)
        .and_then(|stream| nails::client_handle_connection(stream, cmd, stdio_write, stdin_read))
        .map_err(|e| format!("Error communicating with server: {}", e));

    debug!("Connecting to server at {}...", addr);
    let exit_code = runtime
        .block_on(future::join(connection, stdio_printer))
        .0?;
    debug!("Exiting with {}", exit_code.0);
    std::process::exit(exit_code.0);
}
