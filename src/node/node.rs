use std::{collections::HashSet, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

use bitcoin::{
    block::Header,
    p2p::{
        message_filter::{CFHeaders, CFilter},
        Address, ServiceFlags,
    },
    Block, Network,
};
use rand::{prelude::SliceRandom, rngs::StdRng, SeedableRng};
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::Mutex;

use crate::{
    chain::{chain::Chain, checkpoints::HeaderCheckpoint, error::HeaderSyncError},
    node::{error::PersistenceError, peer_map::PeerMap},
    peers::dns::Dns,
    tx::memory::MemoryTransactionCache,
};

use super::{
    channel_messages::{
        GetBlockConfig, GetHeaderConfig, MainThreadMessage, PeerMessage, PeerThreadMessage,
        RemoteVersion,
    },
    client::Client,
    config::NodeConfig,
    error::NodeError,
    node_messages::NodeMessage,
};
use crate::db::sqlite::peer_db::SqlitePeerDb;

#[derive(Debug, Clone, Copy)]
pub enum NodeState {
    // We need to sync headers to the known tip
    Behind,
    // We need to start getting filter headers
    HeadersSynced,
    // We need to get the CP filters
    FilterHeadersSynced,
    // We can start asking for blocks with matches
    FiltersSynced,
    // We found all known transactions to the wallet
    TransactionsSynced,
}

pub struct Node {
    state: Arc<Mutex<NodeState>>,
    chain: Arc<Mutex<Chain>>,
    peer_db: Arc<Mutex<SqlitePeerDb>>,
    best_known_height: u32,
    required_peers: usize,
    white_list: Option<Vec<(IpAddr, u16)>>,
    network: Network,
    client_sender: Sender<NodeMessage>,
}

impl Node {
    pub(crate) async fn new(
        network: Network,
        white_list: Option<Vec<(IpAddr, u16)>>,
        addresses: Vec<bitcoin::Address>,
        data_path: Option<PathBuf>,
        header_checkpoint: Option<HeaderCheckpoint>,
        _required_peers: usize,
    ) -> Result<(Self, Client), NodeError> {
        let (ntx, nrx) = mpsc::channel::<NodeMessage>(32);
        let client = Client::new(nrx);
        let state = Arc::new(Mutex::new(NodeState::Behind));
        let peer_db = SqlitePeerDb::new(network, None)
            .map_err(|_| NodeError::LoadError(PersistenceError::PeerLoadFailure))?;
        let peer_db = Arc::new(Mutex::new(peer_db));
        let mut scripts = HashSet::new();
        scripts.extend(addresses.iter().map(|address| address.script_pubkey()));
        let in_memory_cache = MemoryTransactionCache::new();
        let loaded_chain = Chain::new(
            &network,
            scripts,
            data_path,
            in_memory_cache,
            header_checkpoint,
            ntx.clone(),
        )
        .await
        .map_err(|_| NodeError::LoadError(PersistenceError::HeaderLoadError))?;
        let best_known_height = loaded_chain.height() as u32;
        let chain = Arc::new(Mutex::new(loaded_chain));
        let _ = ntx
            .send(NodeMessage::Dialog(format!(
                "Starting sync from {}",
                best_known_height
            )))
            .await;
        Ok((
            Self {
                state,
                chain,
                peer_db,
                best_known_height,
                required_peers: 1,
                white_list,
                network,
                client_sender: ntx,
            },
            client,
        ))
    }

    pub(crate) async fn new_from_config(
        config: &NodeConfig,
        network: Network,
    ) -> Result<(Self, Client), NodeError> {
        Node::new(
            network,
            config.white_list.clone(),
            config.addresses.clone(),
            config.data_path.clone(),
            config.header_checkpoint,
            config.required_peers as usize,
        )
        .await
    }

    pub async fn run(&mut self) -> Result<(), NodeError> {
        self.send_dialog("Starting node".into()).await;
        let (mtx, mut mrx) = mpsc::channel::<PeerThreadMessage>(32);
        let mut node_map = PeerMap::new(mtx, self.network.clone());
        loop {
            self.advance_state().await;
            node_map.clean().await;
            // Rehydrate on peers when lower than a threshold
            if node_map.live() < self.next_required_peers().await {
                self.send_dialog(format!(
                    "Required peers: {}, connected peers: {}",
                    self.required_peers,
                    node_map.live()
                ))
                .await;
                self.send_dialog("Not connected to enough peers, finding one...".into())
                    .await;
                let ip = self.next_peer().await?;
                node_map.dispatch(ip.0, ip.1).await
            }
            if let Some(block_request) = self.pop_block_queue().await {
                self.send_dialog("Sending block request to a random peer".into())
                    .await;
                node_map.send_random(block_request).await;
            }
            while let Ok(Some(peer_thread)) =
                tokio::time::timeout(Duration::from_secs(1), mrx.recv()).await
            {
                match peer_thread.message {
                    PeerMessage::Version(version) => {
                        node_map.set_offset(peer_thread.nonce, version.timestamp);
                        node_map.set_services(peer_thread.nonce, version.service_flags);
                        let response = self.handle_version(version).await;
                        node_map.send_message(peer_thread.nonce, response).await;
                        self.send_dialog(format!("[Peer {}]: version", peer_thread.nonce))
                            .await;
                    }
                    PeerMessage::Addr(addresses) => self.handle_new_addrs(addresses).await,
                    PeerMessage::Headers(headers) => match self.handle_headers(headers).await {
                        Some(response) => {
                            node_map.send_message(peer_thread.nonce, response).await;
                        }
                        None => continue,
                    },
                    PeerMessage::FilterHeaders(cf_headers) => {
                        match self.handle_cf_headers(peer_thread.nonce, cf_headers).await {
                            Some(response) => {
                                // match depending on disconnect
                                node_map.broadcast(response).await;
                            }
                            None => continue,
                        }
                    }
                    PeerMessage::Filter(filter) => {
                        match self.handle_filter(peer_thread.nonce, filter).await {
                            Some(response) => {
                                node_map.broadcast(response).await;
                            }
                            None => continue,
                        }
                    }
                    PeerMessage::Block(block) => match self.handle_block(block).await {
                        Some(response) => {
                            node_map.broadcast(response).await;
                        }
                        None => continue,
                    },
                    PeerMessage::NewBlocks(_blocks) => match self.handle_inventory_blocks().await {
                        Some(response) => {
                            node_map.broadcast(response).await;
                        }
                        None => continue,
                    },
                    PeerMessage::Disconnect => {
                        node_map.clean().await;
                    }
                    _ => continue,
                }
                self.advance_state().await;
            }
        }
    }

    async fn advance_state(&mut self) {
        let mut state = self.state.lock().await;
        match *state {
            NodeState::Behind => {
                let mut header_guard = self.chain.lock().await;
                if header_guard.is_synced() {
                    self.send_dialog("Headers synced. Auditing our chain with peers".into())
                        .await;
                    header_guard.flush_to_disk().await;
                    *state = NodeState::HeadersSynced;
                }
                return;
            }
            NodeState::HeadersSynced => {
                let header_guard = self.chain.lock().await;
                if header_guard.is_cf_headers_synced() {
                    self.send_dialog("CF Headers synced. Downloading block filters.".into())
                        .await;
                    *state = NodeState::FilterHeadersSynced;
                }
                return;
            }
            NodeState::FilterHeadersSynced => {
                let header_guard = self.chain.lock().await;
                if header_guard.is_filters_synced() {
                    self.send_dialog("Filters synced. Checking blocks for new inclusions.".into())
                        .await;
                    *state = NodeState::FiltersSynced;
                }
                return;
            }
            NodeState::FiltersSynced => {
                let header_guard = self.chain.lock().await;
                if header_guard.block_queue_empty() {
                    *state = NodeState::TransactionsSynced;
                    let _ = self.client_sender.send(NodeMessage::Synced).await;
                }
                return;
            }
            NodeState::TransactionsSynced => return,
        }
    }

    async fn next_required_peers(&self) -> usize {
        let state = self.state.lock().await;
        match *state {
            NodeState::Behind => 1,
            _ => self.required_peers,
        }
    }

    async fn handle_version(&mut self, version_message: RemoteVersion) -> MainThreadMessage {
        let state = self.state.lock().await;
        match *state {
            NodeState::Behind => (),
            _ => {
                if !version_message
                    .service_flags
                    .has(ServiceFlags::COMPACT_FILTERS)
                    || !version_message.service_flags.has(ServiceFlags::NETWORK)
                {
                    self.send_warning(
                        "Connected peer does not serve compact filters or blocks".into(),
                    )
                    .await;
                    return MainThreadMessage::Disconnect;
                }
            }
        }
        // Even if we start the node as caught up in terms of height, we need to check for reorgs
        let mut guard = self.chain.lock().await;
        let peer_height = version_message.height as u32;
        if peer_height.ge(&self.best_known_height) {
            self.best_known_height = peer_height;
            guard.set_best_known_height(peer_height).await;
        }
        let next_headers = GetHeaderConfig {
            locators: guard.locators(),
            stop_hash: None,
        };
        let response = MainThreadMessage::GetHeaders(next_headers);
        response
    }

    async fn handle_new_addrs(&mut self, new_peers: Vec<Address>) {
        let mut guard = self.peer_db.lock().await;
        if let Err(e) = guard.add_cpf_peers(new_peers).await {
            self.send_warning(format!(
                "Encountered error adding peer to persistence: {}",
                e.to_string()
            ))
            .await;
        }
    }

    async fn handle_headers(&mut self, headers: Vec<Header>) -> Option<MainThreadMessage> {
        let mut guard = self.chain.lock().await;
        if let Err(e) = guard.sync_chain(headers).await {
            match e {
                HeaderSyncError::EmptyMessage => {
                    if !guard.is_synced() {
                        return Some(MainThreadMessage::Disconnect);
                    } else if !guard.is_cf_headers_synced() {
                        return Some(MainThreadMessage::GetFilterHeaders(
                            guard.next_cf_header_message().await.unwrap(),
                        ));
                    }
                    return None;
                }
                _ => {
                    self.send_warning(format!(
                        "Unexpected header syncing error: {}",
                        e.to_string()
                    ))
                    .await;
                    return Some(MainThreadMessage::Disconnect);
                }
            }
        }
        if !guard.is_synced() {
            let next_headers = GetHeaderConfig {
                locators: guard.locators(),
                stop_hash: None,
            };
            return Some(MainThreadMessage::GetHeaders(next_headers));
        } else if !guard.is_cf_headers_synced() {
            return Some(MainThreadMessage::GetFilterHeaders(
                guard.next_cf_header_message().await.unwrap(),
            ));
        } else if !guard.is_filters_synced() {
            return Some(MainThreadMessage::GetFilters(
                guard.next_filter_message().await.unwrap(),
            ));
        }
        None
    }

    async fn handle_cf_headers(
        &mut self,
        peer_id: u32,
        cf_headers: CFHeaders,
    ) -> Option<MainThreadMessage> {
        let mut guard = self.chain.lock().await;
        match guard.sync_cf_headers(peer_id, cf_headers).await {
            Ok(potential_message) => match potential_message {
                Some(message) => Some(MainThreadMessage::GetFilterHeaders(message)),
                None => {
                    if !guard.is_filters_synced() {
                        return Some(MainThreadMessage::GetFilters(
                            guard.next_filter_message().await.unwrap(),
                        ));
                    } else {
                        return None;
                    }
                }
            },
            Err(e) => {
                self.send_warning(format!(
                    "Compact filter header syncing encountered an error: {}",
                    e.to_string()
                ))
                .await;
                return Some(MainThreadMessage::Disconnect);
            }
        }
    }

    async fn handle_filter(&mut self, _peer_id: u32, filter: CFilter) -> Option<MainThreadMessage> {
        let mut guard = self.chain.lock().await;
        match guard.sync_filter(filter).await {
            Ok(potential_message) => match potential_message {
                Some(message) => Some(MainThreadMessage::GetFilters(message)),
                None => None,
            },
            Err(e) => {
                self.send_warning(format!(
                    "Compact filter syncing encountered an error: {}",
                    e.to_string()
                ))
                .await;
                Some(MainThreadMessage::Disconnect)
            }
        }
    }

    async fn handle_block(&mut self, block: Block) -> Option<MainThreadMessage> {
        let state = *self.state.lock().await;
        let mut guard = self.chain.lock().await;
        match state {
            NodeState::Behind => Some(MainThreadMessage::Disconnect),
            NodeState::HeadersSynced => {
                // do something with the block to resolve a conflict
                None
            }
            NodeState::FilterHeadersSynced => None,
            NodeState::FiltersSynced => {
                if let Err(e) = guard.scan_block(&block).await {
                    self.send_warning(format!(
                        "Unexpected block scanning error: {}",
                        e.to_string()
                    ))
                    .await;
                }
                None
            }
            NodeState::TransactionsSynced => None,
        }
    }

    async fn pop_block_queue(&mut self) -> Option<MainThreadMessage> {
        let mut guard = self.chain.lock().await;
        let next_block_hash = guard.next_block();
        match next_block_hash {
            Some(block_hash) => {
                self.send_dialog(format!("Next block in queue: {}", block_hash.to_string()))
                    .await;
                Some(MainThreadMessage::GetBlock(GetBlockConfig {
                    locator: block_hash,
                }))
            }
            None => None,
        }
    }

    async fn handle_inventory_blocks(&mut self) -> Option<MainThreadMessage> {
        let mut state = self.state.lock().await;
        match *state {
            NodeState::Behind => return None,
            _ => {
                let guard = self.chain.lock().await;
                *state = NodeState::Behind;
                let next_headers = GetHeaderConfig {
                    locators: guard.locators(),
                    stop_hash: None,
                };
                return Some(MainThreadMessage::GetHeaders(next_headers));
            }
        }
    }

    // First we seach the whitelist for peers that we trust. Then, depending on the state
    // we either need to catch up on block headers or we may start requesting filters and blocks.
    // When requesting filters, we try to select peers that have signaled for CPF support.
    async fn next_peer(&mut self) -> Result<(IpAddr, Option<u16>), NodeError> {
        let state = *self.state.lock().await;
        match state {
            NodeState::Behind => self.any_peer().await,
            _ => match self.cpf_peer().await {
                Ok(got_peer) => match got_peer {
                    Some(peer) => Ok(peer),
                    None => self.any_peer().await,
                },
                Err(e) => return Err(e),
            },
        }
        // self.any_peer().await
    }

    async fn cpf_peer(&mut self) -> Result<Option<(IpAddr, Option<u16>)>, NodeError> {
        let mut guard = self.peer_db.lock().await;
        if let Some(peer) = guard
            .get_random_cpf_peer()
            .await
            .map_err(|_| NodeError::LoadError(PersistenceError::PeerLoadFailure))?
        {
            return Ok(Some((peer.0, Some(peer.1))));
        }
        Ok(None)
    }

    async fn any_peer(&mut self) -> Result<(IpAddr, Option<u16>), NodeError> {
        // empty the whitelist if there is one
        if let Some(whitelist) = &mut self.white_list {
            match whitelist.pop() {
                Some((ip, port)) => {
                    return {
                        self.send_dialog("Using a peer from the white list".into())
                            .await;
                        Ok((ip, Some(port)))
                    }
                }
                None => (),
            }
        }
        let mut guard = self.peer_db.lock().await;
        // try to get any new peer
        let next_peer = guard
            .get_random_new()
            .await
            .map_err(|_| NodeError::LoadError(PersistenceError::PeerLoadFailure))?;
        match next_peer {
            // we found some peer to use but may not be reachable
            Some(peer) => {
                self.send_dialog(format!(
                    "Loaded peer from the database {}",
                    peer.0.to_string()
                ))
                .await;
                Ok((peer.0, Some(peer.1)))
            }
            // we have no peers in our DB, try DNS
            None => {
                let mut new_peers = Dns::bootstrap(self.network)
                    .await
                    .map_err(|_| NodeError::DnsFailure)?;
                let mut rng = StdRng::from_entropy();
                new_peers.shuffle(&mut rng);
                // DNS fails if there is an insufficient number of peers
                let ret_ip = new_peers[0];
                for peer in new_peers {
                    if let Err(e) = guard.add_new(peer, None, None).await {
                        self.send_warning(format!(
                            "Encountered error adding a peer to the database: {}",
                            e.to_string()
                        ))
                        .await;
                    }
                }
                Ok((ret_ip, None))
            }
        }
    }

    async fn send_dialog(&self, message: String) {
        let _ = self.client_sender.send(NodeMessage::Dialog(message)).await;
    }

    async fn send_warning(&self, message: String) {
        let _ = self.client_sender.send(NodeMessage::Warning(message)).await;
    }
}
