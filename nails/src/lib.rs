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
    ///
    /// Spawns an instance of the nail, and returns true if the instance would like to receive
    /// stdin. If stdin should not be accepted, the input_stream will close immediately.
    ///
    fn spawn(
        &self,
        cmd: Command,
        output_sink: mpsc::Sender<ChildOutput>,
        input_stream: mpsc::Receiver<ChildInput>,
    ) -> Result<bool, io::Error>;
}

pub async fn server_handle_connection<N: Nail>(
    config: Config<N>,
    socket: TcpStream,
) -> Result<(), io::Error> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.into_split();
    server_proto::execute(read, write, config).await?;
    Ok(())
}

pub async fn client_handle_connection(
    socket: TcpStream,
    cmd: Command,
    output_sink: mpsc::Sender<ChildOutput>,
    input_stream: mpsc::Receiver<ChildInput>,
) -> Result<ExitCode, io::Error> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.into_split();
    client_proto::execute(read, write, cmd, output_sink, input_stream).await
}

#[cfg(test)]
mod tests {
    use super::{client_handle_connection, server_handle_connection, Config, Nail};

    use crate::execution::{child_channel, ChildInput, ChildOutput, Command, ExitCode};

    use std::io;
    use std::path::PathBuf;

    use log::error;

    use bytes::Bytes;
    use futures::channel::mpsc;
    use futures::{FutureExt, SinkExt, StreamExt, TryFutureExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn roundtrip_noop() {
        let expected_exit_code = ExitCode(67);
        let addr = one_connection_server(Config::new(ConstantNail(expected_exit_code))).await;

        // This Nail will ignore the content of the command, so we're only validating the exit code.
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };
        let (_stdin_write, stdin_read) = child_channel::<ChildInput>();
        let (stdio_write, _stdio_read) = child_channel::<ChildOutput>();
        let exit_code = TcpStream::connect(&addr)
            .and_then(move |stream| client_handle_connection(stream, cmd, stdio_write, stdin_read))
            .map_err(|e| format!("Error communicating with server: {}", e))
            .await
            .unwrap();

        assert_eq!(expected_exit_code, exit_code);
    }

    #[tokio::test]
    async fn roundtrip_echo() {
        let addr = one_connection_server(Config::new(StdoutEchoNail)).await;

        // This Nail ignores the command and echos one blob of stdin.
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };
        let (mut stdin_write, stdin_read) = child_channel::<ChildInput>();
        let (stdio_write, mut stdio_read) = child_channel::<ChildOutput>();

        // This channel has some buffer which we add to before actually launching the client.
        let expected_bytes = Bytes::from("some bytes");
        stdin_write
            .send(ChildInput::Stdin(expected_bytes.clone()))
            .await
            .unwrap();
        stdin_write.send(ChildInput::StdinEOF).await.unwrap();
        let exit_code = TcpStream::connect(&addr)
            .and_then(move |stream| client_handle_connection(stream, cmd, stdio_write, stdin_read))
            .map_err(|e| format!("Error communicating with server: {}", e))
            .await
            .unwrap();

        assert_eq!(
            ChildOutput::Stdout(expected_bytes),
            stdio_read.next().await.unwrap()
        );
        assert_eq!(ExitCode(0), exit_code);
    }

    ///
    /// A server that is spawned into the background, accepts one connection and then exits.
    ///
    async fn one_connection_server<N: Nail>(config: Config<N>) -> std::net::SocketAddr {
        let mut listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let socket = listener.incoming().next().await.unwrap().unwrap();
            println!("Got connection: {:?}", socket);
            tokio::spawn(server_handle_connection(config.clone(), socket))
        });

        addr
    }

    #[derive(Clone)]
    struct ConstantNail(ExitCode);

    impl Nail for ConstantNail {
        fn spawn(
            &self,
            _: Command,
            mut output_sink: mpsc::Sender<ChildOutput>,
            _: mpsc::Receiver<ChildInput>,
        ) -> Result<bool, io::Error> {
            let code = self.0.clone();
            tokio::spawn(async move {
                output_sink.send(ChildOutput::Exit(code)).map(|_| ()).await;
            });
            Ok(false)
        }
    }

    #[derive(Clone)]
    struct StdoutEchoNail;

    impl Nail for StdoutEchoNail {
        fn spawn(
            &self,
            _: Command,
            mut output_sink: mpsc::Sender<ChildOutput>,
            mut input_stream: mpsc::Receiver<ChildInput>,
        ) -> Result<bool, io::Error> {
            tokio::spawn(async move {
                error!("Server spawned thread!");
                let input_bytes = match input_stream.next().await {
                    Some(ChildInput::Stdin(bytes)) => bytes,
                    x => panic!("Unexpected input: {:?}", x),
                };
                match input_stream.next().await {
                    Some(ChildInput::StdinEOF) => (),
                    x => panic!("Unexpected input: {:?}", x),
                };
                output_sink
                    .send(ChildOutput::Stdout(input_bytes))
                    .await
                    .unwrap();
                output_sink
                    .send(ChildOutput::Exit(ExitCode(0)))
                    .await
                    .unwrap();
            });
            Ok(true)
        }
    }
}
