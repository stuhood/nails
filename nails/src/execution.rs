use futures;

use std::fmt::Debug;
use std::io;
use std::path::PathBuf;

use bytes::Bytes;
use futures::sync::mpsc;

const BUF_COUNT: usize = 128;

pub type Args = Vec<String>;
pub type Env = Vec<(String, String)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(pub i32);

#[derive(Debug, Default)]
pub struct Command {
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: PathBuf,
}

#[derive(Debug)]
pub enum ChildInput {
    Stdin(Bytes),
    StdinEOF,
}

#[derive(Debug)]
pub enum ChildOutput {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(ExitCode),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(BUF_COUNT)
}

pub fn send_to_io<T: Debug>(e: mpsc::SendError<T>) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        format!("Failed to send: {:?}", e),
    )
}

///
/// Helper to consume the Unit error of an mpsc Receiver (which cannot trigger).
///
pub fn unreachable_io(_: ()) -> io::Error {
    unreachable!()
}
