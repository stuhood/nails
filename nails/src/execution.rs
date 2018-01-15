extern crate futures;

use std::fmt::Debug;
use std::io;

use bytes::Bytes;
use futures::sync::mpsc;

const BUF_COUNT: usize = 128;

#[derive(Debug, Default)]
pub struct Args {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
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
    Exit(i32),
}

///
/// Creates a channel with a buffer appropriately sized for ChildOutput events.
///
pub fn child_channel<T>() -> (mpsc::Sender<T>, mpsc::Receiver<T>) {
    mpsc::channel(BUF_COUNT)
}

pub fn send_to_io<T: Debug>(e: mpsc::SendError<T>) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("Failed to send: {:?}", e))
}

///
/// Helper to consume the Unit error of an mpsc Receiver (which cannot trigger).
///
pub fn unreachable_io(_: ()) -> io::Error {
    unreachable!()
}
