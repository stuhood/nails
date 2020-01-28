mod client_proto;
mod codec;
pub mod execution;
mod server_proto;

use futures::channel::mpsc;
use std::io;
use tokio::net::TcpStream;

use crate::execution::{ChildInput, ChildOutput, Command, ExitCode};

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
    ) -> Result<(), io::Error>;
}

pub async fn server_handle_connection<N: Nail>(
    config: Config<N>,
    mut socket: TcpStream,
) -> Result<(), io::Error> {
    socket.set_nodelay(true)?;

    let (read, write) = socket.split();
    server_proto::execute(read, write, config).await?;

    Ok(())
}

pub async fn client_handle_connection(
    mut socket: TcpStream,
    cmd: Command,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
) -> Result<ExitCode, io::Error> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.split();
    client_proto::execute(read, write, cmd, output_sink, input_stream).await
}

#[cfg(test)]
mod tests {
    use super::{client_handle_connection, server_handle_connection, Config, Nail};

    use std::io;
    use std::path::PathBuf;

    use crate::execution::{child_channel, ChildInput, ChildOutput, Command, ExitCode};
    use futures::channel::mpsc;
    use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn roundtrip() {
        let expected_exit_code = ExitCode(67);

        // Launch a server that will accept one connection before exiting.
        let config = Config::new(ConstantNail(expected_exit_code));
        let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let socket = listener.incoming().next().await.unwrap().unwrap();
            println!("Got connection: {:?}", socket);
            tokio::spawn(server_handle_connection(config.clone(), socket))
        });

        // And connect with a client. This Nail will ignore the content of the command, so we're
        // only validating the exit code.
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };
        let (stdio_write, _stdio_read) = child_channel::<ChildOutput>();
        let (_stdin_write, stdin_read) = child_channel::<ChildInput>();
        let exit_code = TcpStream::connect(&addr)
            .and_then(move |stream| client_handle_connection(stream, cmd, stdio_write, stdin_read))
            .map_err(|e| format!("Error communicating with server: {}", e))
            .await
            .unwrap();

        assert_eq!(expected_exit_code, exit_code);
    }

    #[derive(Clone)]
    struct ConstantNail(ExitCode);

    impl Nail for ConstantNail {
        fn spawn(
            &self,
            _: Command,
            mut output_sink: mpsc::Sender<ChildOutput>,
            _: mpsc::Receiver<ChildInput>,
        ) -> Result<(), io::Error> {
            let code = self.0.clone();
            tokio::spawn(async move {
                output_sink.send(ChildOutput::Exit(code)).map(|_| ()).await;
            });
            Ok(())
        }
    }
}
