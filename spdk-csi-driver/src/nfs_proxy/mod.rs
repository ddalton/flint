//! flint-nfs-proxy: many lite hubs behind one NFSv4.1 port
//! (docs/plans/flint-lite-nfs-proxy-design.md). The proxy terminates
//! client sessions and routes each COMPOUND to at most one hub by its
//! current filehandle; op bytes are forwarded as the client sent them.

pub mod route;
pub mod wire;
pub mod pseudo;
pub mod backend;
pub mod table;
pub mod server;
pub mod kube;
