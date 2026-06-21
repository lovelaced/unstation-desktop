//! Headless volunteer seed / relay (also embedded in the desktop app for Seed Mode).
//!
//! D7 deliverable. For now it just reports the engine version so the binary builds.

fn main() {
    println!(
        "unstation-node {} — headless seed/relay (stub; see milestone D7)",
        env!("CARGO_PKG_VERSION")
    );
}
