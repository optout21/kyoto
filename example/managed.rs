//! Kyoto supports checking filters directly, as protocols like silent payments will have
//! many possible scripts to check. Enable the `filter-control` feature to check filters
//! manually in your program.

use kyoto::core::messages::NodeMessage;
use kyoto::{chain::checkpoints::HeaderCheckpoint, core::builder::NodeBuilder};
use kyoto::{AddrV2, Address, Network, ServiceFlags, TrustedPeer};
use std::collections::HashSet;
use std::{net::Ipv4Addr, str::FromStr};

const NETWORK: Network = Network::Signet;
const RECOVERY_HEIGHT: u32 = 170_000;
const ADDR: &str = "tb1q9pvjqz5u5sdgpatg3wn0ce438u5cyv85lly0pc";

#[tokio::main]
async fn main() {
    // Add third-party logging
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    // Use a predefined checkpoint
    let checkpoint = HeaderCheckpoint::closest_checkpoint_below_height(RECOVERY_HEIGHT, NETWORK);
    // Add Bitcoin scripts to scan the blockchain for
    let address = Address::from_str(ADDR)
        .unwrap()
        .require_network(NETWORK)
        .unwrap()
        .into();
    let mut addresses = HashSet::new();
    addresses.insert(address);
    // Add preferred peers to connect to
    let peer = TrustedPeer::new(
        AddrV2::Ipv4(Ipv4Addr::new(23, 137, 57, 100)),
        None,
        ServiceFlags::P2P_V2,
    );
    // Create a new node builder
    let builder = NodeBuilder::new(NETWORK);
    // Add node preferences and build the node/client
    let (node, client) = builder
        // Add the peers
        .add_peer(peer)
        // Only scan blocks strictly after an anchor checkpoint
        .anchor_checkpoint(checkpoint)
        // The number of connections we would like to maintain
        .num_required_peers(1)
        // Create the node and client
        .build_node()
        .unwrap();

    tokio::task::spawn(async move { node.run().await });

    let (sender, mut receiver) = client.split();
    // Continually listen for events until the node is synced to its peers.
    loop {
        if let Ok(message) = receiver.recv().await {
            match message {
                NodeMessage::Dialog(d) => tracing::info!("{d}"),
                NodeMessage::Warning(e) => tracing::warn!("{e}"),
                NodeMessage::Synced(_) => break,
                NodeMessage::ConnectionsMet => {
                    tracing::info!("Connected to all required peers");
                }
                NodeMessage::IndexedFilter(mut filter) => {
                    let height = filter.height();
                    tracing::info!("Checking filter {}", height);
                    if filter.contains_any(&addresses).await {
                        let hash = filter.block_hash();
                        tracing::info!("Found script at {}!", hash);
                        break;
                    }
                }
                _ => (),
            }
        }
    }
    let _ = sender.shutdown().await;
    tracing::info!("Shutting down");
}
