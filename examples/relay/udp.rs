//! Runnable UDP relay demo.
//!
//! Same shape as the TCP demo, but the service and tunnel carry UDP datagrams (each
//! source address is demultiplexed into its own overlay flow). It is the *same* pure
//! relay protocol — only the [`TransportKind`](rings_node::extension::transport) differs.
//!
//! Run with: `cargo run -p rings-relay-example --example rings-udp-relay-example`

#[tokio::main]
async fn main() {
    println!("Rings UDP relay demo: client tunnel ⇄ overlay ⇄ server echo service");
    match rings_relay_example::udp_round_trip(b"Hello, Rings UDP relay!").await {
        Ok(got) => println!("relay echoed back: {:?}", String::from_utf8_lossy(&got)),
        Err(e) => {
            eprintln!("demo did not complete (it needs overlay connectivity): {e}");
            std::process::exit(1);
        }
    }
}
