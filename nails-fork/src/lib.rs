use std::io;
use std::process::Stdio;
use std::sync::Arc;

use futures::{stream, FutureExt, SinkExt, StreamExt, TryStreamExt};
use tokio::process::Command;
use tokio::sync::Notify;

use nails::execution::{
    self, child_channel, sink_for, stream_for, ChildInput, ChildOutput, ExitCode,
};
use nails::{server, Nail};

/// A Nail implementation that forks processes.
#[derive(Clone)]
pub struct ForkNail;

impl Nail for ForkNail {
    fn spawn(&self, cmd: execution::Command) -> Result<server::Child, io::Error> {
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
        let (stdin_write, stdin_read) = child_channel::<ChildInput>();
        let stdin = child.stdin.take().unwrap();
        tokio::spawn(async move {
            let mut input_stream = stdin_read.map(|child_input| match child_input {
                ChildInput::Stdin(bytes) => Ok(bytes),
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
        let output_stream = stream::select(stdout_stream, stderr_stream).boxed();

        let killed = Arc::new(Notify::new());
        let killed2 = killed.clone();

        let shutdown = async move {
            killed2.notify_waiters();
        };

        let exit_code = async move {
            tokio::select! {
              res = child.wait() => {
                res
              }
              _ = killed.notified() => {
                // Kill the child process, and then await it to avoid zombies.
                child.kill().await?;
                child.wait().await
              }
            }
        };

        Ok(server::Child::new(
            output_stream,
            Some(stdin_write),
            exit_code
                .map(|res| match res {
                    Ok(exit_status) => ExitCode(exit_status.code().unwrap_or(-1)),
                    Err(_) => ExitCode(-1),
                })
                .boxed(),
            Some(shutdown.boxed()),
        ))
    }
}
