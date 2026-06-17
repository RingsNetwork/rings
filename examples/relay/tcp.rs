//! Runnable TCP relay demo.
//!
//! Spins up two in-process Rings nodes, links them over the overlay, exposes a local
//! TCP echo service on the *server* node under the name `echo`, opens a forwarding
//! tunnel on the *client* node, then talks plain TCP to the tunnel and prints what the
//! relay echoed back.
//!
//! ```text
//!   TcpStream ─▶ client tunnel ─▶ overlay ─▶ server `echo` service ─▶ echo server
//!             ◀────────────────────── echoed bytes ──────────────────────┘
//! ```
//!
//! Run with: `cargo run -p rings-relay-example --example rings-tcp-relay-example`

#[tokio::main]
async fn main() {
    println!("Rings TCP relay demo: client tunnel ⇄ overlay ⇄ server echo service");
    match rings_relay_example::tcp_round_trip(b"Hello, Rings TCP relay!").await {
        Ok(got) => println!("relay echoed back: {:?}", String::from_utf8_lossy(&got)),
        Err(e) => {
            eprintln!("demo did not complete (it needs overlay connectivity): {e}");
            std::process::exit(1);
        }
    }
}
