# Nails

A [nailgun protocol](http://martiansoftware.com/nailgun/protocol.html) server in rust, using [tokio](https://tokio.rs/).

This repository contains three crates:

1. `nails` - contains the nailgun protocol and the `Nail` trait that consuming crates can implement to plug in the logic that they would like to keep warm in the nailgun server.
2. `nails-fork` - contains a `Nail` trait implementation that forks a process as the user running the server.
3. `nails-example` - an example binary that configures and launches a server using `nails-fork`.

Users will generally want to use consume the `nails` crate, but copy-pasting the `nails-fork` crate is helpful to bootstrap your own new `Nail` trait implementation.
