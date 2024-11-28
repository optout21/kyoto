extern crate alloc;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use bitcoin::{
    block::Header,
    p2p::message_filter::{CFHeaders, CFilter, GetCFHeaders, GetCFilters},
    Block, BlockHash, CompactTarget, Network, ScriptBuf, TxOut, Work,
};
use tokio::sync::Mutex;

use super::{
    block_queue::BlockQueue,
    checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
    error::{BlockScanError, HeaderSyncError},
    header_chain::HeaderChain,
};
#[cfg(feature = "filter-control")]
use crate::IndexedFilter;
use crate::{
    chain::header_batch::HeadersBatch,
    core::{
        dialog::Dialog,
        error::HeaderPersistenceError,
        messages::{NodeMessage, Warning},
    },
    db::traits::HeaderStore,
    filters::{
        cfheader_batch::CFHeaderBatch,
        cfheader_chain::{AppendAttempt, CFHeaderChain, QueuedCFHeader},
        error::{CFHeaderSyncError, CFilterSyncError},
        filter_chain::FilterChain,
        Filter, CF_HEADER_BATCH_SIZE, FILTER_BATCH_SIZE,
    },
    prelude::MEDIAN_TIME_PAST,
    IndexedBlock,
};

const MAX_REORG_DEPTH: u32 = 5_000;
const REORG_LOOKBACK: u32 = 7;
const MAX_HEADER_SIZE: usize = 20_000;
const FILTER_BASIC: u8 = 0x00;

#[derive(Debug)]
pub(crate) struct Chain<H: HeaderStore> {
    header_chain: HeaderChain,
    cf_header_chain: CFHeaderChain,
    filter_chain: FilterChain,
    checkpoints: HeaderCheckpoints,
    network: Network,
    db: Arc<Mutex<H>>,
    best_known_height: Option<u32>,
    scripts: HashSet<ScriptBuf>,
    block_queue: BlockQueue,
    dialog: Dialog,
}

#[allow(dead_code)]
impl<H: HeaderStore> Chain<H> {
    pub(crate) fn new(
        network: Network,
        scripts: HashSet<ScriptBuf>,
        anchor: HeaderCheckpoint,
        filter_startpoint_opt: Option<u32>,
        checkpoints: HeaderCheckpoints,
        dialog: Dialog,
        db: H,
        quorum_required: usize,
    ) -> Self {
        let header_chain = HeaderChain::new(anchor);
        let cf_header_chain = CFHeaderChain::new(anchor, quorum_required);
        // Use filter startpoint, or the checkpoint height
        let filter_anchor = filter_startpoint_opt.unwrap_or(anchor.height);
        let filter_chain = FilterChain::new(filter_anchor);
        Chain {
            header_chain,
            checkpoints,
            network,
            db: Arc::new(Mutex::new(db)),
            cf_header_chain,
            filter_chain,
            best_known_height: None,
            scripts,
            block_queue: BlockQueue::new(),
            dialog,
        }
    }

    // Top of the chain
    pub(crate) fn tip(&self) -> BlockHash {
        self.header_chain.tip()
    }

    // The canoncial height of the chain, one less than the length
    pub(crate) fn height(&self) -> u32 {
        self.header_chain.height()
    }

    // This header chain contains a block hash in memory
    pub(crate) fn contains_hash(&self, blockhash: BlockHash) -> bool {
        self.header_chain.contains_hash(blockhash)
    }

    // This header chain contains a block hash, potentially checking the disk
    pub(crate) async fn height_of_hash(&self, blockhash: BlockHash) -> Option<u32> {
        match self.header_chain.height_of_hash(blockhash).await {
            Some(height) => Some(height),
            None => {
                let mut lock = self.db.lock().await;
                lock.height_of(&blockhash).await.unwrap_or(None)
            }
        }
    }

    // This header chain contains a block hash in memory
    pub(crate) fn cached_header_at_height(&self, height: u32) -> Option<&Header> {
        self.header_chain.header_at_height(height)
    }

    // Fetch a header from the cache or disk.
    pub(crate) async fn fetch_header(
        &mut self,
        height: u32,
    ) -> Result<Option<Header>, HeaderPersistenceError<H::Error>> {
        match self.header_chain.header_at_height(height) {
            Some(header) => Ok(Some(*header)),
            None => {
                let mut db = self.db.lock().await;
                let header_opt = db.header_at(height).await;
                if header_opt.is_err() {
                    self.dialog
                        .send_warning(Warning::FailedPersistance {
                            warning: format!(
                                "Unexpected error fetching a header from the header store at height {height}"
                            ),
                        })
                        .await;
                }
                header_opt.map_err(HeaderPersistenceError::Database)
            }
        }
    }

    // The hash at the given height, potentially checking on disk
    pub(crate) async fn blockhash_at_height(&self, height: u32) -> Option<BlockHash> {
        match self
            .cached_header_at_height(height)
            .map(|header| header.block_hash())
        {
            Some(hash) => Some(hash),
            None => {
                let mut lock = self.db.lock().await;
                lock.hash_at(height).await.unwrap_or(None)
            }
        }
    }

    // This header chain contains a block hash
    pub(crate) fn contains_header(&self, header: &Header) -> bool {
        self.header_chain.contains_header(header)
    }

    // Canoncial chainwork
    pub(crate) fn chainwork(&self) -> Work {
        self.header_chain.chainwork()
    }

    // Calculate the chainwork after a fork height to evalutate the fork
    pub(crate) fn chainwork_after_height(&self, height: u32) -> Work {
        self.header_chain.chainwork_after_height(height)
    }

    // Human readable chainwork
    pub(crate) fn log2_work(&self) -> f64 {
        self.header_chain.log2_work()
    }

    // Have we hit the known checkpoints
    pub(crate) fn checkpoints_complete(&self) -> bool {
        self.checkpoints.is_exhausted()
    }

    // The last ten heights and headers in the chain
    pub(crate) fn last_ten(&self) -> BTreeMap<u32, Header> {
        self.header_chain.last_ten()
    }

    // Set the best known height to our peer
    pub(crate) async fn set_best_known_height(&mut self, height: u32) {
        self.dialog
            .send_dialog(format!("Best known peer height: {}", height))
            .await;
        self.best_known_height = Some(height);
    }

    // Do we have best known height and is our height equal to it
    // If our height is greater, we received partial inventory, and
    // the header message contained the rest of the new blocks.
    pub(crate) fn is_synced(&mut self) -> bool {
        if let Some(best) = self.best_known_height {
            let current = self.height();
            if current > best {
                self.best_known_height = Some(current);
                true
            } else {
                current.eq(&best)
            }
        } else {
            false
        }
    }

    // The "locators" are the headers we inform our peers we know about
    pub(crate) async fn locators(&mut self) -> Vec<BlockHash> {
        // If a peer is sending us a fork at this point they are faulty.
        if !self.checkpoints_complete() {
            vec![self.tip()]
        } else {
            // We should try to catch any reorgs if we are on a fresh start.
            // The database may have a header that is useful to the remote node
            // that is not currently in memory.
            if self.header_chain.inner_len() < REORG_LOOKBACK as usize {
                let older_locator = self.height().saturating_sub(REORG_LOOKBACK);
                let mut db_lock = self.db.lock().await;
                let hash = db_lock.hash_at(older_locator).await;
                if let Ok(Some(locator)) = hash {
                    vec![self.tip(), locator]
                } else {
                    // We couldn't find a header deep enough to send over. Just proceed as usual
                    self.header_chain.locators()
                }
            } else {
                // We have enough headers in memory to catch a reorg.
                self.header_chain.locators()
            }
        }
    }

    // Write the chain to disk
    pub(crate) async fn flush_to_disk(&mut self) {
        if let Err(e) = self
            .db
            .lock()
            .await
            .write(self.header_chain.headers())
            .await
        {
            self.dialog
                .send_warning(Warning::FailedPersistance {
                    warning: format!("Could not save headers to disk: {e}"),
                })
                .await;
        }
    }

    // Write the chain to disk, overriding previous heights
    pub(crate) async fn flush_over_height(&mut self, height: u32) {
        if let Err(e) = self
            .db
            .lock()
            .await
            .write_over(self.header_chain.headers(), height)
            .await
        {
            self.dialog
                .send_warning(Warning::FailedPersistance {
                    warning: format!("Could not save headers to disk: {e}"),
                })
                .await;
        }
    }

    // Load in the headers
    pub(crate) async fn load_headers(&mut self) -> Result<(), HeaderPersistenceError<H::Error>> {
        let loaded_headers = self
            .db
            .lock()
            .await
            .load_after(self.height())
            .await
            .map_err(HeaderPersistenceError::Database)?;
        if let Some(first) = loaded_headers.values().next() {
            if first.prev_blockhash.ne(&self.tip()) {
                self.dialog.send_warning(Warning::UnlinkableAnchor).await;
                // The header chain did not align, so just start from the anchor
                return Err(HeaderPersistenceError::CannotLocateHistory);
            } else if loaded_headers
                .iter()
                .zip(loaded_headers.iter().skip(1))
                .any(|(first, second)| first.1.block_hash().ne(&second.1.prev_blockhash))
            {
                self.dialog.send_warning(Warning::CorruptedHeaders).await;
                return Err(HeaderPersistenceError::HeadersDoNotLink);
            }
            loaded_headers.iter().for_each(|header| {
                if let Some(checkpoint) = self.checkpoints.next() {
                    if header.1.block_hash().eq(&checkpoint.hash) {
                        self.checkpoints.advance()
                    }
                }
            })
        }
        self.header_chain.set_headers(loaded_headers);
        Ok(())
    }

    // If the number of headers in memory gets too large, move some of them to the disk
    pub(crate) async fn manage_memory(&mut self) {
        if self.header_chain.inner_len() > MAX_HEADER_SIZE {
            self.flush_to_disk().await;
            self.header_chain.move_up();
        }
    }

    // Sync the chain with headers from a peer, adjusting to reorgs if needed
    pub(crate) async fn sync_chain(&mut self, message: Vec<Header>) -> Result<(), HeaderSyncError> {
        let header_batch = HeadersBatch::new(message).map_err(|_| HeaderSyncError::EmptyMessage)?;
        // If our chain already has the last header in the message there is no new information
        if self.contains_hash(header_batch.last().block_hash()) {
            return Ok(());
        }
        // We check first if the peer is sending us nonsense
        self.sanity_check(&header_batch).await?;
        // How we handle forks depends on if we are caught up through all checkpoints or not
        match self.checkpoints.next().cloned() {
            Some(checkpoint) => {
                self.catch_up_sync(header_batch, checkpoint).await?;
            }
            None => {
                // Nothing left to do but add the headers to the chain
                if self.tip().eq(&header_batch.first().prev_blockhash) {
                    self.audit_difficulty(self.height(), &header_batch).await?;
                    self.header_chain.extend(header_batch.inner());
                    return Ok(());
                }
                // We see if we have this previous hash in the database, and reload our
                // chain from that hash if so.
                let fork_start_hash = header_batch.first().prev_blockhash;
                if !self.contains_hash(fork_start_hash) {
                    self.load_fork(&header_batch).await?;
                }
                // Check if the fork has more work.
                self.evaluate_fork(&header_batch).await?;
            }
        };
        self.manage_memory().await;
        Ok(())
    }

    // These are invariants in all batches of headers we receive
    async fn sanity_check(&mut self, header_batch: &HeadersBatch) -> Result<(), HeaderSyncError> {
        let initially_syncing = !self.checkpoints.is_exhausted();
        // Some basic sanity checks that should result in peer bans on errors

        // If we aren't synced up to the checkpoints we don't accept any forks
        if initially_syncing && self.tip().ne(&header_batch.first().prev_blockhash) {
            return Err(HeaderSyncError::PreCheckpointFork);
        }

        // All the headers connect with each other and is the difficulty adjustment not absurd
        if !header_batch.connected().await {
            return Err(HeaderSyncError::HeadersNotConnected);
        }

        // All headers pass their own proof of work and the network minimum
        if !header_batch.individually_valid_pow().await {
            return Err(HeaderSyncError::InvalidHeaderWork);
        }

        if !header_batch.bits_adhere_transition(self.network).await {
            return Err(HeaderSyncError::InvalidBits);
        }

        // The headers have times that are greater than the median of the previous 11 blocks
        let mut last_relevant_mtp = self.header_chain.last_median_time_past_window();
        if !header_batch
            .valid_median_time_past(&mut last_relevant_mtp)
            .await
        {
            // The first validation may be incorrect because of median miscalculation,
            // but it is cheap to detect the peer is lying later from checkpoints
            // and difficulty of the SHA256 algorithm
            if self.header_chain.inner_len() > MEDIAN_TIME_PAST {
                return Err(HeaderSyncError::InvalidHeaderTimes);
            }
        }
        Ok(())
    }

    /// Sync with extra requirements on checkpoints and forks
    async fn catch_up_sync(
        &mut self,
        header_batch: HeadersBatch,
        checkpoint: HeaderCheckpoint,
    ) -> Result<(), HeaderSyncError> {
        self.audit_difficulty(self.height(), &header_batch).await?;
        // Eagerly append the batch to the chain
        self.header_chain.extend(header_batch.inner());
        // We need to check a hard-coded checkpoint
        if self.height().ge(&checkpoint.height) {
            if self
                .blockhash_at_height(checkpoint.height)
                .await
                .ok_or(HeaderSyncError::InvalidCheckpoint)?
                .eq(&checkpoint.hash)
            {
                self.dialog
                    .send_dialog(format!("Found checkpoint, height: {}", checkpoint.height))
                    .await;
                self.dialog.send_dialog("Writing progress to disk...").await;
                self.checkpoints.advance();
                self.flush_to_disk().await;
            } else {
                self.dialog
                    .send_warning(
                        Warning::UnexpectedSyncError { warning: "Unmatched checkpoint sent by a peer. Restarting header sync with new peers.".into() }
                    )
                    .await;
                return Err(HeaderSyncError::InvalidCheckpoint);
            }
        }
        Ok(())
    }

    // Audit the difficulty adjustment of the blocks we received

    // This function draws from the neutrino implemention, where even if a fork is valid
    // we only accept it if there is more work provided. otherwise, we disconnect the peer sending
    // us this fork
    async fn evaluate_fork(&mut self, header_batch: &HeadersBatch) -> Result<(), HeaderSyncError> {
        self.dialog.send_warning(Warning::EvaluatingFork).await;
        // We only care about the headers these two chains do not have in common
        let uncommon: Vec<Header> = header_batch
            .inner()
            .iter()
            .filter(|header| !self.contains_header(header))
            .copied()
            .collect();
        let challenge_chainwork = uncommon
            .iter()
            .map(|header| header.work())
            .reduce(|acc, next| acc + next)
            .ok_or(HeaderSyncError::FloatingHeaders)?;
        let stem_position = self
            .height_of_hash(
                uncommon
                    .first()
                    .ok_or(HeaderSyncError::FloatingHeaders)?
                    .prev_blockhash,
            )
            .await;
        if let Some(stem) = stem_position {
            let current_chainwork = self.header_chain.chainwork_after_height(stem);
            if current_chainwork.lt(&challenge_chainwork) {
                self.dialog.send_dialog("Valid reorganization found").await;
                let reorged = self.header_chain.extend(&uncommon);
                let removed_hashes = &reorged
                    .iter()
                    .map(|disconnect| disconnect.header.block_hash())
                    .collect::<Vec<BlockHash>>();
                self.clear_compact_filter_queue();
                self.cf_header_chain.remove(removed_hashes);
                self.filter_chain.remove(removed_hashes);
                self.block_queue.remove(removed_hashes);
                self.dialog
                    .send_data(NodeMessage::BlocksDisconnected(reorged))
                    .await;
                self.flush_over_height(stem).await;
                Ok(())
            } else {
                self.dialog
                    .send_warning(Warning::UnexpectedSyncError {
                        warning: "Peer sent us a fork with less work than the current chain".into(),
                    })
                    .await;
                Err(HeaderSyncError::LessWorkFork)
            }
        } else {
            Err(HeaderSyncError::FloatingHeaders)
        }
    }

    async fn audit_difficulty(
        &mut self,
        height_start: u32,
        batch: &HeadersBatch,
    ) -> Result<(), HeaderSyncError> {
        let params = self.network.params();
        if params.no_pow_retargeting {
            return Ok(());
        }
        if params.allow_min_difficulty_blocks {
            return Ok(());
        }
        // Next adjustment height = (floor(current height / interval) + 1) * interval
        let adjustment_interval = params.difficulty_adjustment_interval() as u32;
        let next_multiple = (height_start / adjustment_interval) + 1;
        let next_adjustment_height = next_multiple * adjustment_interval;
        // The height in the batch where the next adjustment is contained
        let offset = next_adjustment_height - height_start;
        // We already audited the difficulty last batch
        if offset == 0 {
            return Ok(());
        }
        // We cannot audit the difficulty yet, as the next adjustment will be contained in the next batch
        if offset > batch.len() as u32 {
            if let Some(tip) = self.cached_header_at_height(height_start) {
                if batch.inner().iter().any(|header| header.bits.ne(&tip.bits)) {
                    self.dialog
                    .send_warning(Warning::UnexpectedSyncError {
                        warning:
                            "The remote peer miscalculated the difficulty adjustment when syncing a batch of headers"
                                .into(),
                    })
                    .await;
                    return Err(HeaderSyncError::MiscalculatedDifficulty);
                }
            }
            return Ok(());
        }
        // The difficulty should be adjusted at this height
        let audit_index = (offset - 1) as usize;
        // This is the timestamp used to start the boundary
        let last_epoch_start_index = next_adjustment_height - adjustment_interval;
        // This is the timestamp used to end the boundary
        let last_epoch_boundary = if offset == 1 {
            // This is the case where the last epoch ends on the tip of our chain
            self.fetch_header(height_start).await.ok().flatten()
        } else {
            // Otherwise we can simply index into the batch and find the header at the boundary'
            let last_epoch_boundary_index = (offset - 2) as usize;
            batch.get(last_epoch_boundary_index).copied()
        };
        // The start of the epoch will always be a member of our chain because the batch size
        // is less than the adjustment interval
        let last_epoch_start = self
            .fetch_header(last_epoch_start_index)
            .await
            .ok()
            .flatten();

        let audit = batch.get_slice(audit_index..);

        match audit {
            Some(headers) => match last_epoch_start.zip(last_epoch_boundary) {
                Some((first, second)) => {
                    let target =
                        CompactTarget::from_header_difficulty_adjustment(first, second, params);
                    for header in headers {
                        let retarget_bits = header.bits;
                        if retarget_bits.ne(&target) {
                            self.dialog
                                .send_warning(Warning::UnexpectedSyncError {
                                    warning:
                                        "The remote peer miscalculated the difficulty adjustment when syncing a batch of headers"
                                            .into(),
                                })
                                .await;
                            return Err(HeaderSyncError::MiscalculatedDifficulty);
                        }
                    }
                    return Ok(());
                }
                None => {
                    self.dialog
                        .send_dialog(
                            "Unable to audit the difficulty adjustment due to a failed header fetch...",
                        )
                        .await;
                    self.dialog
                        .send_dialog("This is likely due to no history present in the header store")
                        .await;
                }
            },
            None => {
                self.dialog
                    .send_warning(Warning::UnexpectedSyncError {
                        warning:
                            "Unable to audit the difficulty adjustment due to an index overflow"
                                .into(),
                    })
                    .await;
            }
        }
        Ok(())
    }

    // We don't have a header in memory that we need to evaluate a fork.
    // We check if we have it on disk, and load some more headers into memory.
    // This call occurs if we sync to a block that is later reorganized out of the chain,
    // but we have restarted our node in between these events.
    async fn load_fork(&mut self, header_batch: &HeadersBatch) -> Result<(), HeaderSyncError> {
        let prev_hash = header_batch.first().prev_blockhash;
        let maybe_height = {
            let mut db_lock = self.db.lock().await;
            db_lock
                .height_of(&prev_hash)
                .await
                .map_err(|_| HeaderSyncError::DbError)?
        };
        match maybe_height {
            Some(height) => {
                // This is a very generous check to ensure a peer cannot get us to load an
                // absurd amount of headers into RAM. Because headers come in batches of 2,000,
                // we wouldn't accept a fork of a depth more than around 2,000 anyway.
                // The only reorgs that have ever been recorded are of depth 1.
                if self.height() - height > MAX_REORG_DEPTH {
                    return Err(HeaderSyncError::FloatingHeaders);
                } else {
                    let older_anchor = HeaderCheckpoint::new(height, prev_hash);
                    self.header_chain = HeaderChain::new(older_anchor);
                    self.cf_header_chain =
                        CFHeaderChain::new(older_anchor, self.cf_header_chain.quorum_required());
                    self.filter_chain = FilterChain::new(older_anchor.height);
                }
            }
            None => return Err(HeaderSyncError::FloatingHeaders),
        }
        self.load_headers()
            .await
            .map_err(|_| HeaderSyncError::DbError)?;
        Ok(())
    }

    // Sync the compact filter headers, possibly encountering conflicts
    pub(crate) async fn sync_cf_headers(
        &mut self,
        _peer_id: u32,
        cf_headers: CFHeaders,
    ) -> Result<AppendAttempt, CFHeaderSyncError> {
        let mut batch: CFHeaderBatch = cf_headers.into();
        self.dialog
            .chain_update(
                self.height(),
                self.cf_header_chain.height(),
                self.filter_chain.height(),
                self.best_known_height.unwrap_or(self.height()),
            )
            .await;
        match batch.last_header() {
            Some(batch_last) => {
                if let Some(prev_header) = self.cf_header_chain.prev_header() {
                    // A new block was mined and we ended up asking for this batch twice,
                    // or the quorum required is less than our connected peers.
                    if batch_last.eq(&prev_header) {
                        return Ok(AppendAttempt::AddedToQueue);
                    }
                }
            }
            None => return Err(CFHeaderSyncError::EmptyMessage),
        }
        // Check for any obvious faults
        self.audit_cf_headers(&batch).await?;
        // We already have a message like this. Verify they are the same
        if self.cf_header_chain.has_queue() {
            Ok(self.cf_header_chain.verify(&mut batch).await)
        } else {
            // Associate the block hashes with the filter hashes and add them to the queue
            let queue = self.construct_cf_header_queue(&mut batch).await?;
            Ok(self.cf_header_chain.set_queue(queue).await)
        }
    }

    // We need to associate the block hash with the incoming filter hashes
    async fn construct_cf_header_queue(
        &self,
        batch: &mut CFHeaderBatch,
    ) -> Result<Vec<QueuedCFHeader>, CFHeaderSyncError> {
        let mut queue = Vec::new();
        let ref_height = self.cf_header_chain.height();
        for (index, (filter_header, filter_hash)) in batch.take_inner().into_iter().enumerate() {
            let block_hash = self
                // This call may or may not retrieve the hash from disk
                .blockhash_at_height(ref_height + index as u32 + 1)
                .await
                .ok_or(CFHeaderSyncError::HeaderChainIndexOverflow)?;
            queue.push(QueuedCFHeader::new(block_hash, filter_header, filter_hash))
        }
        Ok(queue)
    }

    // Audit the validity of a batch of compact filter headers
    async fn audit_cf_headers(&mut self, batch: &CFHeaderBatch) -> Result<(), CFHeaderSyncError> {
        // Does the filter header line up with our current chain of filter headers
        if let Some(prev_header) = self.cf_header_chain.prev_header() {
            if batch.prev_header().ne(&prev_header) {
                return Err(CFHeaderSyncError::PrevHeaderMismatch);
            }
        }
        // Did we request up to this stop hash. We should have caught if this was a repeated message.
        let prev_stophash = self
            .cf_header_chain
            .last_stop_hash_request()
            .ok_or(CFHeaderSyncError::UnexpectedCFHeaderMessage)?;
        if prev_stophash.ne(batch.stop_hash()) {
            return Err(CFHeaderSyncError::StopHashMismatch);
        }
        // Did they send us the right amount of headers
        let stop_hash =
            // This call may or may not retrieve the hash from disk
            self.blockhash_at_height(self.cf_header_chain.height() + batch.len() as u32)
            .await
            .ok_or(CFHeaderSyncError::HeaderChainIndexOverflow)?;
        if stop_hash.ne(batch.stop_hash()) {
            return Err(CFHeaderSyncError::StopHashMismatch);
        }
        Ok(())
    }

    // We need to make this public for new peers that connect to us throughout syncing the filter headers
    pub(crate) async fn next_cf_header_message(&mut self) -> GetCFHeaders {
        let stop_hash_index = self.cf_header_chain.height() + CF_HEADER_BATCH_SIZE + 1;
        let stop_hash = self
            .blockhash_at_height(stop_hash_index)
            .await
            .unwrap_or(self.tip());
        self.cf_header_chain.set_last_stop_hash(stop_hash);
        GetCFHeaders {
            filter_type: FILTER_BASIC,
            start_height: self.cf_header_chain.height() + 1,
            stop_hash,
        }
    }

    // Are the compact filter headers caught up to the header chain
    pub(crate) fn is_cf_headers_synced(&self) -> bool {
        self.height().le(&self.cf_header_chain.height())
    }

    // Handle a new filter
    pub(crate) async fn sync_filter(
        &mut self,
        filter_message: CFilter,
    ) -> Result<Option<GetCFilters>, CFilterSyncError> {
        if self.is_filters_synced() {
            return Ok(None);
        }
        let mut filter = Filter::new(filter_message.filter, filter_message.block_hash);
        let expected_filter_hash = self.cf_header_chain.hash_at(&filter_message.block_hash);
        // Disallow any filter that we do not have a block hash for
        match expected_filter_hash {
            Some(ref_hash) => {
                if filter.filter_hash().ne(ref_hash) {
                    return Err(CFilterSyncError::MisalignedFilterHash);
                }
            }
            None => {
                return Err(CFilterSyncError::UnknownFilterHash);
            }
        }

        #[cfg(feature = "filter-control")]
        {
            let height = self
                .height_of_hash(filter_message.block_hash)
                .await
                .ok_or(CFilterSyncError::UnknownFilterHash)?;
            let indexed_filter = IndexedFilter::new(height, filter);
            self.dialog
                .send_data(NodeMessage::IndexedFilter(indexed_filter))
                .await;
        }

        #[cfg(not(feature = "filter-control"))]
        if !self.block_queue.contains(&filter_message.block_hash)
            && filter
                .contains_any(&self.scripts)
                .await
                .map_err(CFilterSyncError::Filter)?
        {
            // Add to the block queue
            self.block_queue.add(filter_message.block_hash);
            self.dialog
                .send_dialog(format!(
                    "Found script at block: {}",
                    filter_message.block_hash
                ))
                .await;
        }

        self.filter_chain.put_hash(filter_message.block_hash).await;
        let stop_hash = self
            .filter_chain
            .last_stop_hash_request()
            .ok_or(CFilterSyncError::UnrequestedStophash)?;
        if filter_message.block_hash.eq(&stop_hash) {
            if !self.is_filters_synced() {
                Ok(Some(self.next_filter_message().await))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    // Next filter message, if there is one
    pub(crate) async fn next_filter_message(&mut self) -> GetCFilters {
        let stop_hash_index = self.filter_chain.height() + FILTER_BATCH_SIZE + 1;
        let stop_hash = self
            .blockhash_at_height(stop_hash_index)
            .await
            .unwrap_or(self.tip());
        self.dialog
            .chain_update(
                self.height(),
                self.cf_header_chain.height(),
                self.filter_chain.height(),
                self.best_known_height.unwrap_or(self.height()),
            )
            .await;
        self.filter_chain.set_last_stop_hash(stop_hash);
        GetCFilters {
            filter_type: FILTER_BASIC,
            start_height: self.filter_chain.height() + 1,
            stop_hash,
        }
    }

    // Are we synced with filters
    pub(crate) fn is_filters_synced(&self) -> bool {
        self.height().le(&self.filter_chain.height())
    }

    // Pop a block from the queue of interesting blocks
    pub(crate) fn next_block(&mut self) -> Option<BlockHash> {
        self.block_queue.pop()
    }

    // Are there any blocks left in the queue
    pub(crate) fn block_queue_empty(&self) -> bool {
        self.block_queue.complete()
    }

    // Make sure we have this hash in our chain, check the merkle root, and pass the block
    pub(crate) async fn check_send_block(&mut self, block: Block) -> Result<(), BlockScanError> {
        let block_hash = block.block_hash();
        if !self.block_queue.need(&block_hash) {
            return Ok(());
        }
        let height = self
            .height_of_hash(block_hash)
            .await
            .ok_or(BlockScanError::NoBlockHash)?;
        if !block.check_merkle_root() {
            return Err(BlockScanError::InvalidMerkleRoot);
        }
        self.block_queue.receive(&block_hash);
        self.dialog
            .send_data(NodeMessage::Block(IndexedBlock::new(height, block)))
            .await;
        Ok(())
    }

    // Should we care about this block
    #[allow(dead_code)]
    fn scan_outputs(&mut self, inputs: &[TxOut]) -> bool {
        inputs
            .iter()
            .any(|out| self.scripts.contains(&out.script_pubkey))
    }

    // Add a script to our list
    pub(crate) fn put_script(&mut self, script: ScriptBuf) {
        self.scripts.insert(script);
    }

    // Explicitly request a block
    #[cfg(feature = "filter-control")]
    pub(crate) fn get_block(&mut self, hash: BlockHash) {
        self.block_queue.add(hash)
    }

    // Reset the compact filter queue because we received a new block
    pub(crate) fn clear_compact_filter_queue(&mut self) {
        self.cf_header_chain.clear_queue();
    }

    // We found a reorg and some filters are no longer valid.
    async fn clear_filter_headers(&mut self) {
        self.cf_header_chain.clear_queue();
        self.cf_header_chain.clear_headers();
    }

    // Clear the filter header cache to rescan the filters for new scripts.
    pub(crate) async fn clear_filters(&mut self) {
        self.filter_chain.clear_cache().await;
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, str::FromStr};

    use bitcoin::{
        block::Header,
        consensus::deserialize,
        p2p::message_filter::{CFHeaders, CFilter},
        BlockHash, FilterHash, FilterHeader,
    };
    use bitcoin_hashes::sha256d;
    use bitcoin_hashes::Hash;

    use crate::{
        chain::{
            checkpoints::{HeaderCheckpoint, HeaderCheckpoints},
            error::HeaderSyncError,
        },
        core::{dialog::Dialog, messages::NodeMessage},
        filters::cfheader_chain::AppendAttempt,
    };

    use super::Chain;

    fn new_regtest(anchor: HeaderCheckpoint) -> Chain<()> {
        let (sender, _) = tokio::sync::broadcast::channel::<NodeMessage>(1);
        let mut checkpoints = HeaderCheckpoints::new(&bitcoin::Network::Regtest);
        checkpoints.prune_up_to(anchor);
        Chain::new(
            bitcoin::Network::Regtest,
            HashSet::new(),
            anchor,
            None,
            checkpoints,
            Dialog::new(sender),
            (),
            1,
        )
    }

    fn new_regtest_two_peers(anchor: HeaderCheckpoint) -> Chain<()> {
        let (sender, _) = tokio::sync::broadcast::channel::<NodeMessage>(1);
        let mut checkpoints = HeaderCheckpoints::new(&bitcoin::Network::Regtest);
        checkpoints.prune_up_to(anchor);
        Chain::new(
            bitcoin::Network::Regtest,
            HashSet::new(),
            anchor,
            None,
            checkpoints,
            Dialog::new(sender),
            (),
            2,
        )
    }

    #[tokio::test]
    async fn test_depth_one_fork() {
        let gen = HeaderCheckpoint::new(
            7,
            BlockHash::from_str("62c28f380692524a3a8f1fc66252bc0eb31d6b6a127d2263bdcbee172529fe16")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_8: Header = deserialize(&hex::decode("0000002016fe292517eecbbd63227d126a6b1db30ebc5262c61f8f3a4a529206388fc262dfd043cef8454f71f30b5bbb9eb1a4c9aea87390f429721e435cf3f8aa6e2a9171375166ffff7f2000000000").unwrap()).unwrap();
        let block_9: Header = deserialize(&hex::decode("000000205708a90197d93475975545816b2229401ccff7567cb23900f14f2bd46732c605fd8de19615a1d687e89db365503cdf58cb649b8e935a1d3518fa79b0d408704e71375166ffff7f2000000000").unwrap()).unwrap();
        let block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093790c9f554a7780a6043a19619d2a4697364bb62abf6336c0568c31f1eedca3c3e171375166ffff7f2000000000").unwrap()).unwrap();
        let batch_1 = vec![block_8, block_9, block_10];
        let new_block_10: Header = deserialize(&hex::decode("000000201d062f2162835787db536c55317e08df17c58078c7610328bdced198574093792151c0e9ce4e4c789ca98427d7740cc7acf30d2ca0c08baef266bf152289d814567e5e66ffff7f2001000000").unwrap()).unwrap();
        let block_11: Header = deserialize(&hex::decode("00000020efcf8b12221fccc735b9b0b657ce15b31b9c50aff530ce96a5b4cfe02d8c0068496c1b8a89cf5dec22e46c35ea1035f80f5b666a1b3aa7f3d6f0880d0061adcc567e5e66ffff7f2001000000").unwrap()).unwrap();
        let batch_2 = vec![new_block_10];
        let batch_3 = vec![block_11];
        let batch_4 = vec![block_9, new_block_10, block_11];
        let chain_sync = chain.sync_chain(batch_1).await;
        assert!(chain_sync.is_ok());
        // Forks of equal height to the chain should just get rejected
        let fork_sync = chain.sync_chain(batch_2).await;
        assert!(fork_sync.is_err());
        assert_eq!(fork_sync.err().unwrap(), HeaderSyncError::LessWorkFork);
        assert_eq!(10, chain.height());
        // A peer sent us a block we don't know about yet, but is in the best chain
        // Best we can do is wait to get the fork from another peer
        let float_sync = chain.sync_chain(batch_3).await;
        assert!(float_sync.is_err());
        assert_eq!(float_sync.err().unwrap(), HeaderSyncError::FloatingHeaders);
        assert_eq!(10, chain.height());
        // Now we can accept the fork because it has more work
        let extend_sync = chain.sync_chain(batch_4.clone()).await;
        assert_eq!(11, chain.height());
        assert!(extend_sync.is_ok());
        assert_eq!(
            vec![block_8, block_9, new_block_10, block_11],
            chain.header_chain.values()
        );
        assert_eq!(chain.fetch_header(10).await.unwrap().unwrap(), new_block_10);
        // A new peer sending us these headers should not do anything
        let dup_sync = chain.sync_chain(batch_4).await;
        assert_eq!(11, chain.height());
        assert!(dup_sync.is_ok());
        assert_eq!(
            vec![block_8, block_9, new_block_10, block_11],
            chain.header_chain.values()
        );
    }

    #[tokio::test]
    async fn test_fork_includes_old_vals() {
        let gen = HeaderCheckpoint::new(
            0,
            BlockHash::from_str("0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f047eb4d0fe76345e307d0e020a079cedfa37101ee7ac84575cf829a611b0f84bc4805e66ffff7f2001000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("00000020299e41732deb76d869fcdb5f72518d3784e99482f572afb73068d52134f1f75e1f20f5da8d18661d0f13aa3db8fff0f53598f7d61f56988a6d66573394b2c6ffc5805e66ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea3471f1dbedc283ce6a43a87ed3c8e6326dae8d3dbacce1b2daba08e508054ffdb697815e66ffff7f2001000000").unwrap()).unwrap();
        let batch_1 = vec![block_1, block_2, block_3];
        let new_block_3: Header = deserialize(&hex::decode("00000020b96feaa82716f11befeb608724acee4743e0920639a70f35f1637a88b8b6ea349c6240c5d0521966771808950f796c9c04088bc9551a828b64f1cf06831705dfbc835e66ffff7f2000000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("00000020d2a1c6ba2be393f405fe2f4574565f9ee38ac68d264872fcd82b030970d0232ce882eb47c3dd138587120f1ad97dd0e73d1e30b79559ad516cb131f83dcb87e9bc835e66ffff7f2002000000").unwrap()).unwrap();
        let batch_2 = vec![block_1, block_2, new_block_3, block_4];
        let chain_sync = chain.sync_chain(batch_1).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 3);
        assert_eq!(chain.header_chain.values(), vec![block_1, block_2, block_3]);
        let chain_sync = chain.sync_chain(batch_2).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 4);
        assert_eq!(
            chain.header_chain.values(),
            vec![block_1, block_2, new_block_3, block_4]
        );
    }

    #[tokio::test]
    async fn test_depth_two_fork() {
        let gen = HeaderCheckpoint::new(
            0,
            BlockHash::from_str("0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f575b313ad3ef825cfc204c34da8f3c1fd1784e2553accfa38001010587cb57241f855e66ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("00000020c81cedd6a989939936f31448e49d010a13c2e750acf02d3fa73c9c7ecfb9476e798da2e5565335929ad303fc746acabc812ee8b06139bcf2a4c0eb533c21b8c420855e66ffff7f2000000000").unwrap()).unwrap();
        let batch_1 = vec![block_1, block_2];
        let new_block_1: Header = deserialize(&hex::decode("0000002006226e46111a0b59caaf126043eb5bbf28c34f3a5e332a1fc7b2b73cf188910f575b313ad3ef825cfc204c34da8f3c1fd1784e2553accfa38001010587cb5724d5855e66ffff7f2004000000").unwrap()).unwrap();
        let new_block_2: Header = deserialize(&hex::decode("00000020d1d80f53343a084bd0da6d6ab846f9fe4a133de051ea00e7cae16ed19f601065798da2e5565335929ad303fc746acabc812ee8b06139bcf2a4c0eb533c21b8c4d6855e66ffff7f2000000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("0000002080f38c14e898d6646dd426428472888966e0d279d86453f42edc56fdb143241aa66c8fa8837d95be3f85d53f22e86a0d6d456b1ab348e073da4d42a39f50637423865e66ffff7f2000000000").unwrap()).unwrap();
        let batch_2 = vec![new_block_1];
        let batch_3 = vec![new_block_1, new_block_2];
        let batch_4 = vec![new_block_1, new_block_2, block_3];
        let chain_sync = chain.sync_chain(batch_1).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2);
        assert_eq!(chain.header_chain.values(), vec![block_1, block_2]);
        let chain_sync = chain.sync_chain(batch_2).await;
        assert!(chain_sync.is_err());
        assert_eq!(chain_sync.err().unwrap(), HeaderSyncError::LessWorkFork);
        assert_eq!(chain.height(), 2);
        let chain_sync = chain.sync_chain(batch_3).await;
        assert!(chain_sync.is_err());
        assert_eq!(chain_sync.err().unwrap(), HeaderSyncError::LessWorkFork);
        assert_eq!(chain.height(), 2);
        let chain_sync = chain.sync_chain(batch_4).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 3);
        assert_eq!(chain.fetch_header(3).await.unwrap().unwrap(), block_3);
        assert_eq!(
            chain.header_chain.values(),
            vec![new_block_1, new_block_2, block_3]
        );
    }

    #[tokio::test]
    async fn test_filters_out_of_order() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        let sync_filter_3 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_3.block_hash(),
                filter: filter_3,
            })
            .await;
        assert!(sync_filter_3.is_ok());
        let sync_filter_2 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_2.block_hash(),
                filter: filter_2,
            })
            .await;
        assert!(sync_filter_2.is_ok());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
        assert!(chain.is_filters_synced());
    }

    #[tokio::test]
    async fn test_bad_filter() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_2,
            })
            .await;
        assert!(sync_filter_1.is_err());
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
    }

    #[tokio::test]
    async fn test_bad_blockhash() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            // Wrong block hash
            stop_hash: block_3.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        // Try the request again
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let append_attempt = cf_header_sync_res.unwrap();
        assert_eq!(AppendAttempt::Extended, append_attempt);
        assert!(chain.is_cf_headers_synced());
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_2.block_hash(),
                filter: filter_1.clone(),
            })
            .await;
        assert!(sync_filter_1.is_err());
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
    }

    #[tokio::test]
    async fn test_has_conflict() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_3],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(
            cf_header_sync_res.unwrap(),
            AppendAttempt::Conflict(block_4.block_hash())
        );
        assert!(!chain.cf_header_chain.has_queue());
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(3, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        assert!(chain.is_cf_headers_synced());
    }

    #[tokio::test]
    async fn test_uneven_cf_headers() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        // Not enough filter hashes
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        // Wrong stop hash
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_3.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
    }

    #[tokio::test]
    async fn test_reorg_no_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let new_filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let new_filter_hash_4 = sha256d::Hash::hash(&new_filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let new_filter_hash_4 = FilterHash::from_raw_hash(new_filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2501);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_err());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: new_block_4.block_hash(),
                filter: new_filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_reorg_with_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let new_filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let new_filter_hash_4 = sha256d::Hash::hash(&new_filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let new_filter_hash_4 = FilterHash::from_raw_hash(new_filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2501);
        // Request the CF headers again
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                new_filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
    }

    #[tokio::test]
    async fn reorg_during_filter_sync() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134f84b9962adfb060e7b251a52d0ad0bc13eb6a69d35900860e9e0e027ff2bb86a3462c266ffff7f2001000000").unwrap()).unwrap();
        let new_block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0107dda0").unwrap();
        let new_filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let new_filter_hash_4 = sha256d::Hash::hash(&new_filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let new_filter_hash_4 = FilterHash::from_raw_hash(new_filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(0, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        chain.next_filter_message().await;
        let sync_filter_1 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_1.block_hash(),
                filter: filter_1,
            })
            .await;
        assert!(sync_filter_1.is_ok());
        // Reorganize the blocks
        let header_batch = vec![new_block_4, block_5];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert!(chain.is_synced());
        // Request the headers again
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("4818ea31ceccf249909aad97f1da4f8ec2ca5738fb56b2f8b443b80fe8f91387")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![new_filter_hash_4, filter_hash_5],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::AddedToQueue);
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("4818ea31ceccf249909aad97f1da4f8ec2ca5738fb56b2f8b443b80fe8f91387")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![new_filter_hash_4, filter_hash_5],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: block_4.block_hash(),
                filter: filter_4,
            })
            .await;
        assert!(sync_filter_4.is_err());
        let sync_filter_4 = chain
            .sync_filter(CFilter {
                filter_type: 0x00,
                block_hash: new_block_4.block_hash(),
                filter: new_filter_4,
            })
            .await;
        assert!(sync_filter_4.is_ok());
    }

    #[tokio::test]
    async fn test_inv_no_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        let chain_sync = chain.sync_chain(vec![block_5]).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2501);
        assert!(chain.is_synced());
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_err());
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(2, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
    }

    #[tokio::test]
    async fn test_inv_with_queue() {
        let gen = HeaderCheckpoint::new(
            2496,
            BlockHash::from_str("4b4f478800538b3301b681358f84d870da0f9c4cde63ebd85fa0f273dfb07c6a")
                .unwrap(),
        );
        let mut chain = new_regtest_two_peers(gen);
        let block_1: Header = deserialize(&hex::decode("000000206a7cb0df73f2a05fd8eb63de4c9c0fda70d8848f3581b601338b530088474f4bbe54a272e64276a49cf98359a6e43563b6527cce7c9434c0c2ca21b4710b84593362c266ffff7f2000000000").unwrap()).unwrap();
        let block_2: Header = deserialize(&hex::decode("000000204326468f18d82108c98e5a328192770c8cb8d4e3322a4df708fe3232b3f0797dcd9468dd32ad9d68cfd49048378ec2caae965e4998200e4f83cba92f396f0b373462c266ffff7f2001000000").unwrap()).unwrap();
        let block_3: Header = deserialize(&hex::decode("00000020a860ab5e9320ad1e0318e154ea31cab1e030a1f4e1bcf89c63bfdf3055852d01053e4b600cfa947ce54315cc62b23e706dbfca5566f3156b272bf1f8971d930b3462c266ffff7f2001000000").unwrap()).unwrap();
        let block_4: Header = deserialize(&hex::decode("0000002004a138485264fdcec8abcd044e26a97b501649f941b9eed342ae26c51bfde134fdb874f33a34f746f688c148583d90fe9c5512790a2c0891bb99c7595a7891b52f84c366ffff7f2002000000").unwrap()).unwrap();
        let block_5: Header = deserialize(&hex::decode("0000002085e2486fdb11997b8ecec9f765da62ee5b4c457f6b7903103bcaaeb6149ffe5e2e35eae749a0fa88c203757b8df4c797f71d0d4728389694c405d029a9ad96eb2f84c366ffff7f2000000000").unwrap()).unwrap();
        let header_batch = vec![block_1, block_2, block_3, block_4];
        let chain_sync = chain.sync_chain(header_batch).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2500);
        chain.set_best_known_height(2500).await;
        assert!(chain.is_synced());
        let filter_1 = hex::decode("018976c0").unwrap();
        let filter_2 = hex::decode("018b1f28").unwrap();
        let filter_3 = hex::decode("01117310").unwrap();
        let filter_4 = hex::decode("0189dff0").unwrap();
        let filter_5 = hex::decode("01504fe0").unwrap();
        let filter_hash_1 = sha256d::Hash::hash(&filter_1);
        let filter_hash_2 = sha256d::Hash::hash(&filter_2);
        let filter_hash_3 = sha256d::Hash::hash(&filter_3);
        let filter_hash_4 = sha256d::Hash::hash(&filter_4);
        let filter_hash_5 = sha256d::Hash::hash(&filter_5);
        let filter_hash_1 = FilterHash::from_raw_hash(filter_hash_1);
        let filter_hash_2 = FilterHash::from_raw_hash(filter_hash_2);
        let filter_hash_3 = FilterHash::from_raw_hash(filter_hash_3);
        let filter_hash_4 = FilterHash::from_raw_hash(filter_hash_4);
        let filter_hash_5 = FilterHash::from_raw_hash(filter_hash_5);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_4.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![filter_hash_1, filter_hash_2, filter_hash_3, filter_hash_4],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        let chain_sync = chain.sync_chain(vec![block_5]).await;
        assert!(chain_sync.is_ok());
        assert_eq!(chain.height(), 2501);
        chain.next_cf_header_message().await;
        let cf_headers = CFHeaders {
            filter_type: 0x00,
            stop_hash: block_5.block_hash(),
            previous_filter_header: FilterHeader::from_slice(
                &hex::decode("12c10339861d7ca367696b8c92a4c5acb609e66e5bf2d352376225ead1f78011")
                    .unwrap(),
            )
            .unwrap(),
            filter_hashes: vec![
                filter_hash_1,
                filter_hash_2,
                filter_hash_3,
                filter_hash_4,
                filter_hash_5,
            ],
        };
        let cf_header_sync_res = chain.sync_cf_headers(1, cf_headers).await;
        assert!(cf_header_sync_res.is_ok());
        assert_eq!(cf_header_sync_res.unwrap(), AppendAttempt::Extended);
        assert_eq!(chain.cf_header_chain.height(), 2500);
    }
}
