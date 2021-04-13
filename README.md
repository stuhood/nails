# Nails

A [nailgun protocol](http://martiansoftware.com/nailgun/protocol.html) client/server implementation, using [tokio](https://tokio.rs/).

This repository contains four crates:

1. `nails` - contains nailgun client and server protocols and the `Nail` trait that consuming crates can implement to plug in the logic that they would like to keep warm in the nailgun server.
2. `nails-fork` - contains a `Nail` trait implementation that forks a process as the user running the server.
3. `nails-example` - an example server binary that configures and launches a server using `nails-fork`.
4. `nails-client` - a nailgun client binary, intended to be equivalent to the C and Python clients.

Users will generally want to consume the `nails` crate in order to act as either a client or server. Copy-pasting the `nails-fork` crate might be helpful to bootstrap a new `Nail` trait implementation to host in a server.
