extern crate bytes;
extern crate futures;
extern crate log;
extern crate tokio_codec;
extern crate tokio_core;
extern crate tokio_io;

mod client_proto;
mod codec;
pub mod execution;
mod server_proto;

use futures::sync::mpsc;
use futures::{future, Future};
use std::io;
use tokio_codec::Decoder;
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;

use codec::{ClientCodec, ServerCodec};
use execution::{ChildInput, ChildOutput, Command, ExitCode};

#[derive(Clone)]
pub struct Config<N: Nail> {
    nail: N,
    noisy_stdin: bool,
}

impl<N> Config<N>
where
    N: Nail,
{
    pub fn new(nail: N) -> Config<N> {
        Config {
            nail,
            noisy_stdin: true,
        }
    }

    /// Although it is not part of the spec, the Python and C clients require that
    /// `StartReadingStdin` is sent after every stdin chunk has been consumed.
    ///   see https://github.com/facebook/nailgun/issues/88
    pub fn noisy_stdin(mut self, value: bool) -> Self {
        self.noisy_stdin = value;
        self
    }
}

pub trait Nail: Clone + Send + Sync + 'static {
    fn spawn(
        &self,
        cmd: Command,
        output_sink: mpsc::Sender<ChildOutput>,
        input_stream: mpsc::Receiver<ChildInput>,
        handle: &Handle,
    ) -> Result<(), io::Error>;
}

pub fn server_handle_connection<N: Nail>(
    config: Config<N>,
    handle: &Handle,
    socket: TcpStream,
) -> Result<(), io::Error> {
    socket.set_nodelay(true)?;

    handle.spawn(
        server_proto::execute(handle.clone(), ServerCodec.framed(socket), config).map_err(|_| ()),
    );

    Ok(())
}

pub fn client_handle_connection(
    socket: TcpStream,
    cmd: Command,
) -> Box<dyn Future<Item = ExitCode, Error = io::Error>> {
    if let Err(e) = socket.set_nodelay(true) {
        return Box::new(future::err(e));
    };

    Box::new(client_proto::execute(ClientCodec.framed(socket), cmd))
}

#[cfg(test)]
mod tests {
    use super::{client_handle_connection, server_handle_connection, Config, Nail};

    use std::io;
    use std::path::PathBuf;

    use execution::{ChildInput, ChildOutput, Command, ExitCode};
    use futures::sync::mpsc;
    use futures::{Future, Sink, Stream};
    use tokio_core::net::{TcpListener, TcpStream};
    use tokio_core::reactor::{Core, Handle};

    #[test]
    fn roundtrip() {
        let expected_exit_code = ExitCode(67);
        let mut core = Core::new().unwrap();

        // Launch a server that will accept one connection before exiting.
        let handle = core.handle();
        let config = Config::new(ConstantNail(expected_exit_code));
        let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap(), &handle).unwrap();
        let addr = listener.local_addr().unwrap();

        core.handle().spawn(
            listener
                .incoming()
                .take(1)
                .for_each(move |(socket, _)| {
                    println!("Got connection: {:?}", socket);
                    server_handle_connection(config.clone(), &handle, socket)
                })
                .map_err(|e| panic!("Server exited early: {}", e)),
        );

        // And connect with a client. This Nail will ignore the content of the command, so we're
        // only validating the exit code.
        let handle = core.handle();
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };
        let exit_code = core
            .run(
                TcpStream::connect(&addr, &handle)
                    .and_then(|stream| client_handle_connection(stream, cmd))
                    .map_err(|e| format!("Error communicating with server: {}", e)),
            )
            .unwrap();

        assert_eq!(expected_exit_code, exit_code);
    }

    #[derive(Clone)]
    struct ConstantNail(ExitCode);

    impl Nail for ConstantNail {
        fn spawn(
            &self,
            _: Command,
            output_sink: mpsc::Sender<ChildOutput>,
            _: mpsc::Receiver<ChildInput>,
            handle: &Handle,
        ) -> Result<(), io::Error> {
            handle.spawn(
                output_sink
                    .send(ChildOutput::Exit(self.0.clone()))
                    .map(|_| ())
                    .map_err(|e| panic!("Server could not send an exit code: {}", e)),
            );
            Ok(())
        }
    }
}
