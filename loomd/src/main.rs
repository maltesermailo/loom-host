//! loomd — the Loom host daemon (quinn server, session mgmt, mDNS, config).
//!
//! Stub binary: the daemon is implemented across later milestones. It exists in
//! the workspace now so the layout matches spec/ARCHITECTURE.md §3/§5. It links
//! loom-proto to keep the wire types in the build graph.
fn main() {
    println!(
        "loomd stub — Loom host daemon, protocol version {}",
        loom_proto::PROTOCOL_VERSION
    );
}
