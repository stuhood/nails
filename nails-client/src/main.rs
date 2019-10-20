extern crate nails;

extern crate env_logger;
extern crate futures;
extern crate log;
extern crate tokio_core;

use std::env;
use std::net::SocketAddr;

use futures::Future;
use log::debug;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Core;

use nails::execution::Command;

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

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    debug!("Connecting to server at {}...", addr);
    let exit_code = core.run(
        TcpStream::connect(&addr, &handle)
            .and_then(|stream| nails::client_handle_connection(stream, cmd))
            .map_err(|e| format!("Error communicating with server: {}", e)),
    )?;
    debug!("Exiting with {}", exit_code.0);
    std::process::exit(exit_code.0);
}
