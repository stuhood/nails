pub mod client;
mod codec;
pub mod execution;
pub mod server;

use std::io;
use std::time::Duration;

use crate::execution::Command;

#[derive(Default, Clone)]
pub struct Config {
    noisy_stdin: bool,
    heartbeat_frequency: Option<Duration>,
}

impl Config {
    ///
    /// Although it is not part of the spec, the Python and C clients require that
    /// `StartReadingStdin` is sent after every stdin chunk has been consumed.
    ///   see https://github.com/facebook/nailgun/issues/88
    ///
    pub fn noisy_stdin(mut self, value: bool) -> Self {
        self.noisy_stdin = value;
        self
    }

    ///
    /// The "heartbeat" is an optional protocol extension, and is a chunk type sent by the client to
    /// the server to indicate that the client is still waiting for a result. By default, the
    /// server will not require that heartbeat messages are received, but setting a value here will
    /// cause the server to cancel/Drop the connection if a heartbeat message is not received at at
    /// least this frequency.
    ///
    pub fn heartbeat_frequency(mut self, frequency: Duration) -> Self {
        self.heartbeat_frequency = Some(frequency);
        self
    }
}

pub trait Nail: Clone + Send + Sync + 'static {
    ///
    /// Spawns an instance of the nail and returns a server::Child.
    ///
    fn spawn(&self, cmd: Command) -> Result<server::Child, io::Error>;
}

#[cfg(test)]
mod tests {
    use super::{client, server, Config, Nail};
    use crate::execution::{child_channel, ChildInput, ChildOutput, Command, ExitCode};

    use std::future::Future;
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use bytes::Bytes;
    use futures::stream;
    use futures::{FutureExt, SinkExt, StreamExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;
    use tokio::time::sleep;

    #[tokio::test]
    async fn roundtrip_noop() {
        let _logger = env_logger::try_init();
        let expected_exit_code = ExitCode(67);
        let (addr, _) =
            one_connection_server(Config::default(), ConstantNail(None, expected_exit_code)).await;

        // This Nail will ignore the content of the command, so we're only validating the exit code.
        let exit_code = send_with_no_stdio(
            Config::default(),
            addr,
            Command {
                command: "nothing".to_owned(),
                args: vec![],
                env: vec![],
                working_dir: PathBuf::from("/dev/null"),
            },
        )
        .await
        .unwrap();

        assert_eq!(expected_exit_code, exit_code);
    }

    #[tokio::test]
    async fn roundtrip_echo() {
        let (addr, _) = one_connection_server(Config::default(), StdoutEchoNail).await;

        // This Nail ignores the command and echos one blob of stdin.
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };

        // This channel has some buffer which we add to before actually launching the client.
        let expected_bytes = Bytes::from("some bytes");
        let mut child = {
            let expected_bytes = expected_bytes.clone();
            let stream = TcpStream::connect(&addr).await.unwrap();
            client::handle_connection(Config::default(), stream, cmd, async {
                let (mut stdin_write, stdin_read) = child_channel::<ChildInput>();
                stdin_write
                    .send(ChildInput::Stdin(expected_bytes))
                    .await
                    .unwrap();
                stdin_read
            })
            .await
            .unwrap()
        };

        assert_eq!(
            ChildOutput::Stdout(expected_bytes),
            child.output_stream.take().unwrap().next().await.unwrap()
        );
        assert_eq!(ExitCode(0), child.wait().await.unwrap());
    }

    #[tokio::test]
    async fn roundtrip_cancellation() {
        // Spawn on a nail that will wait a long time before returning, and then cancel
        // the run, and confirm that the client and server half of the connection both exit before
        // the full wait.
        let full_wait = Duration::from_secs(30);
        let deadline = Instant::now() + full_wait;
        let expected_exit_code = ExitCode(123);
        let (addr, server) = one_connection_server(
            Config::default(),
            ConstantNail(Some(full_wait), expected_exit_code),
        )
        .await;
        let cmd = Command {
            command: "nothing".to_owned(),
            args: vec![],
            env: vec![],
            working_dir: PathBuf::from("/dev/null"),
        };

        // Launch the client.
        let mut child = {
            let stream = TcpStream::connect(&addr).await.unwrap();
            client::handle_connection(Config::default(), stream, cmd, async {
                let (_stdin_write, stdin_read) = child_channel::<ChildInput>();
                stdin_read
            })
            .await
            .unwrap()
        };

        // Cancel the client, and confirm that we get a particular ExitCode.
        child.shutdown().await;
        assert_eq!(ExitCode(-2), child.wait().await.unwrap());

        // And that that server exits as well.
        server.await;

        // ... all before the deadline.
        assert!(Instant::now() < deadline);
    }

    #[tokio::test]
    async fn roundtrip_heartbeat_success() {
        // Enforcing a heartbeat timeout shorter than the nail's total runtime should succeed if the
        // client and server are in alignment on heartbeats.
        let config = Config::default().heartbeat_frequency(Duration::from_millis(100));
        roundtrip_heartbeat(config.clone(), config, true).await;
    }

    #[tokio::test]
    async fn roundtrip_heartbeat_failure() {
        // Enforcing a heartbeat timeout shorter than the nail's total runtime should fail if the
        // client is not sending heartbeats, but the server is requiring them.
        roundtrip_heartbeat(
            Config::default(),
            Config::default().heartbeat_frequency(Duration::from_millis(100)),
            false,
        )
        .await;
    }

    async fn roundtrip_heartbeat(
        client_config: Config,
        server_config: Config,
        expect_success: bool,
    ) {
        // Ask the nail to run for a multiple of the heartbeat frequency, so that multiple
        // heartbeats are required to succeed.
        let nail_delay = server_config.heartbeat_frequency.map(|f| f * 4);
        let success_exit_code = ExitCode(67);
        let (addr, _) =
            one_connection_server(server_config, ConstantNail(nail_delay, success_exit_code)).await;

        let exit_code = send_with_no_stdio(
            client_config,
            addr,
            Command {
                command: "nothing".to_owned(),
                args: vec![],
                env: vec![],
                working_dir: PathBuf::from("/dev/null"),
            },
        )
        .await
        .unwrap();

        if expect_success {
            assert_eq!(success_exit_code, exit_code);
        } else {
            assert_eq!(ExitCode(-2), exit_code);
        }
    }

    ///
    /// A server that is spawned into the background, accepts one connection and then exits.
    ///
    async fn one_connection_server(
        config: Config,
        nail: impl Nail,
    ) -> (std::net::SocketAddr, impl Future<Output = ()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let socket = listener.accept().await.unwrap().0;
            println!("Got connection: {:?}", socket);
            server::handle_connection(config.clone(), nail, socket).await
        })
        .map(|_| ());

        (addr, server)
    }

    ///
    /// Send the given command, expecting no stdio in or out.
    ///
    async fn send_with_no_stdio(
        config: Config,
        addr: std::net::SocketAddr,
        command: Command,
    ) -> Result<ExitCode, String> {
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| format!("Error connecting to server: {}", e))?;
        let child = client::handle_connection(config, stream, command, async {
            let (_stdin_write, stdin_read) = child_channel::<ChildInput>();
            stdin_read
        })
        .await
        .map_err(|e| format!("Error launching process: {}", e))?;
        child
            .wait()
            .await
            .map_err(|e| format!("Process exited abnormally: {}", e))
    }

    ///
    /// A Nail that sleeps for the given duration, and then returns the given ExitCode.
    ///
    #[derive(Clone)]
    struct ConstantNail(Option<Duration>, ExitCode);

    impl Nail for ConstantNail {
        fn spawn(&self, _: Command) -> Result<server::Child, io::Error> {
            let nail = self.clone();
            let killed = Arc::new(Notify::new());
            let killed2 = killed.clone();
            let shutdown = async move {
                killed2.notify_waiters();
            };
            let exit_code = async move {
                if let Some(delay_duration) = nail.0 {
                    tokio::select! {
                      _ = sleep(delay_duration) => {
                          // We delayed and then exited successfully.
                          nail.1
                      }
                      _ = killed.notified() => {
                          // We were cancelled: exit immediately unsuccessfully.
                          ExitCode(-2)
                      }
                    }
                } else {
                    // No delay: exit immediately without handling cancellation.
                    nail.1
                }
            };
            Ok(server::Child::new(
                stream::iter(vec![]).boxed(),
                None,
                exit_code.boxed(),
                Some(shutdown.boxed()),
            ))
        }
    }

    #[derive(Clone)]
    struct StdoutEchoNail;

    impl Nail for StdoutEchoNail {
        fn spawn(&self, _: Command) -> Result<server::Child, io::Error> {
            let (stdin_write, mut stdin_read) = child_channel::<ChildInput>();
            let output = async move {
                log::info!("Server spawned thread!");
                let input_bytes = match stdin_read.next().await {
                    Some(ChildInput::Stdin(bytes)) => bytes,
                    x => panic!("Unexpected input: {:?}", x),
                };
                if let Some(x) = stdin_read.next().await {
                    panic!("Unexpected input: {:?}", x);
                };
                stream::iter(vec![Ok(ChildOutput::Stdout(input_bytes))])
            };

            let exit_code = async move { ExitCode(0) };
            Ok(server::Child::new(
                output.into_stream().flatten().boxed(),
                Some(stdin_write),
                exit_code.boxed(),
                None,
            ))
        }
    }
}
