use std::str;

use futures::{Future, Stream};
use tokio_core::net::TcpListener;
use tokio_core::reactor::Core;
use tokio_io::AsyncRead;

use codec::{Codec, InputChunk};

pub fn serve(addr: &str) {
    let mut core = Core::new().unwrap();
    let handle = core.handle();
    let remote_addr = addr.parse().unwrap();

    let listener = TcpListener::bind(&remote_addr, &handle).unwrap();
    println!("Bound listener: {:?}", listener);
    let server = listener.incoming().for_each(move |(socket, _)| {
        println!("Got connection: {:?}", socket);
        let transport = socket.framed(Codec);

        let (execute_chunks, stdin_chunks) =
            transport.split_off(|chunk| match chunk {
                e @ &InputChunk::Execute {..} => {
                    println!("Got execute chunk!: {:?}", e);
                    true
                },
                x => {
                    println!("Got other chunk!: {:?}", x);
                    false
                },
            });

        // Consume the execute chunk, begin executing, and then stream stdin chunks.
        let execution =
            execute_chunks.collect().and_then(|mut execute_chunks| {
                if execute_chunks.len() != 1 {
                    unimplemented!("TODO");
                }
                match execute_chunks.pop().unwrap() {
                    e @ InputChunk::Execute { .. } => {
                        println!("Got {:?}", e);
                        unimplemented!("TODO: Invoke.");
                    },
                    _ => unimplemented!("TODO"),
                }
                Ok(())
            });

        handle.spawn(execution.map_err(|_| ()));

        Ok(())
    });

    core.run(server).unwrap();
}
