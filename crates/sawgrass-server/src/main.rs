//! The Sawgrass world server.
//!
//! A placeholder until the tick loop (SRV-1) and the gateway connection (NET-2) land.

fn main() {
    println!(
        "sawgrass-server {} (protocol {})",
        env!("CARGO_PKG_VERSION"),
        sawgrass_protocol::PROTOCOL_VERSION
    );
}
