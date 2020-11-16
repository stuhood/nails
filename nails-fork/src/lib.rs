use std::io;
use std::process::Stdio;

use futures::channel::mpsc;
use futures::{stream, FutureExt, SinkExt, StreamExt, TryStreamExt};
use tokio::process::Command;

use nails::execution::{self, sink_for, stream_for, ChildInput, ChildOutput, ExitCode};
use nails::{server, Nail};

/// A Nail implementation that forks processes.
#[derive(Clone)]
pub struct ForkNail;

impl Nail for ForkNail {
    fn spawn(
        &self,
        cmd: execution::Command,
        input_stream: mpsc::Receiver<ChildInput>,
    ) -> Result<server::Child, io::Error> {
        let mut child = Command::new(cmd.command.clone())
            .args(cmd.args)
            .env_clear()
            .envs(cmd.env)
            .current_dir(cmd.working_dir)
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::piped())
            .spawn()?;

        // Copy inputs to the child.
        let stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            let mut input_stream = input_stream.filter_map(|child_input| {
                Box::pin(async move {
                    match child_input {
                        ChildInput::Stdin(bytes) => Some(Ok(bytes)),
                        ChildInput::StdinEOF => None,
                    }
                })
            });
            sink_for(stdin)
                .send_all(&mut input_stream)
                .map(|_| ())
                .await;
        });

        // Fully consume the stdout/stderr streams before waiting on the exit stream.
        let stdout_stream = stream_for(child.stdout.take().unwrap())
            .map_ok(|bytes| ChildOutput::Stdout(bytes.into()));
        let stderr_stream = stream_for(child.stderr.take().unwrap())
            .map_ok(|bytes| ChildOutput::Stderr(bytes.into()));
        let exit_stream = child
            .into_stream()
            .map_ok(|exit_status| ChildOutput::Exit(ExitCode(exit_status.code().unwrap_or(-1))));
        let output_stream = stream::select(stdout_stream, stderr_stream)
            .chain(exit_stream)
            .map(|res| match res {
                Ok(o) => o,
                Err(e) => {
                    log::warn!("IO error interacting with child process: {:?}", e);
                    ChildOutput::Exit(ExitCode(-1))
                }
            })
            .boxed();

        Ok(server::Child {
            output_stream,
            accepts_stdin: true,
        })
    }
}
