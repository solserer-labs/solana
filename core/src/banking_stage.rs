//! The `banking_stage` processes Transaction messages. It is intended to be used
//! to contruct a software pipeline. The stage uses all available CPU cores and
//! can do its processing in parallel with signature verification on the GPU.
use crate::{
    cluster_info::ClusterInfo,
    packet_hasher::PacketHasher,
    poh_recorder::{PohRecorder, PohRecorderError, TransactionRecorder, WorkingBankEntry},
    poh_service::{self, PohService},
};
use crossbeam_channel::{Receiver as CrossbeamReceiver, RecvTimeoutError};
use itertools::Itertools;
use lru::LruCache;
use retain_mut::RetainMut;
use solana_ledger::{
    blockstore::Blockstore, blockstore_processor::TransactionStatusSender,
    entry::hash_transactions, leader_schedule_cache::LeaderScheduleCache,
};
use solana_measure::{measure::Measure, thread_mem_usage};
use solana_metrics::{inc_new_counter_debug, inc_new_counter_info};
use solana_perf::{
    cuda_runtime::PinnedVec,
    packet::{limited_deserialize, Packet, Packets, PACKETS_PER_BATCH},
    perf_libs,
};
use solana_runtime::{
    accounts_db::ErrorCounters,
    bank::{
        Bank, ExecuteTimings, TransactionBalancesSet, TransactionCheckResult,
        TransactionExecutionResult,
    },
    bank_utils,
    transaction_batch::TransactionBatch,
    vote_sender_types::ReplayVoteSender,
};
use solana_sdk::{
    clock::{
        Slot, DEFAULT_TICKS_PER_SLOT, MAX_PROCESSING_AGE, MAX_TRANSACTION_FORWARDING_DELAY,
        MAX_TRANSACTION_FORWARDING_DELAY_GPU,
    },
    poh_config::PohConfig,
    pubkey::Pubkey,
    timing::{duration_as_ms, timestamp},
    transaction::{self, Transaction, TransactionError},
};
use solana_transaction_status::token_balances::{
    collect_token_balances, TransactionTokenBalancesSet,
};
use std::{
    cmp,
    collections::{HashMap, VecDeque},
    env,
    net::UdpSocket,
    ops::DerefMut,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    sync::mpsc::Receiver,
    sync::{Arc, Mutex},
    thread::{self, Builder, JoinHandle},
    time::Duration,
    time::Instant,
};

/// (packets, valid_indexes, forwarded)
/// Set of packets with a list of which are valid and if this batch has been forwarded.
type PacketsAndOffsets = (Packets, Vec<usize>, bool);

pub type UnprocessedPackets = VecDeque<PacketsAndOffsets>;

/// Transaction forwarding
pub const FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET: u64 = 2;
pub const HOLD_TRANSACTIONS_SLOT_OFFSET: u64 = 20;

// Fixed thread size seems to be fastest on GCP setup
pub const NUM_THREADS: u32 = 4;

const TOTAL_BUFFERED_PACKETS: usize = 500_000;

const MAX_NUM_TRANSACTIONS_PER_BATCH: usize = 128;

const DEFAULT_LRU_SIZE: usize = 200_000;

#[derive(Debug, Default)]
pub struct BankingStageStats {
    last_report: AtomicU64,
    id: u32,
    process_packets_count: AtomicUsize,
    new_tx_count: AtomicUsize,
    dropped_batches_count: AtomicUsize,
    newly_buffered_packets_count: AtomicUsize,
    current_buffered_packets_count: AtomicUsize,
    rebuffered_packets_count: AtomicUsize,
    consumed_buffered_packets_count: AtomicUsize,
}

impl BankingStageStats {
    pub fn new(id: u32) -> Self {
        BankingStageStats {
            id,
            ..BankingStageStats::default()
        }
    }

    fn report(&self, report_interval_ms: u64) {
        let should_report = {
            let last = self.last_report.load(Ordering::Relaxed);
            let now = solana_sdk::timing::timestamp();
            now.saturating_sub(last) > report_interval_ms
                && self.last_report.compare_exchange(
                    last,
                    now,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) == Ok(last)
        };

        if should_report {
            datapoint_info!(
                "banking_stage-loop-stats",
                ("id", self.id as i64, i64),
                (
                    "process_packets_count",
                    self.process_packets_count.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "new_tx_count",
                    self.new_tx_count.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "dropped_batches_count",
                    self.dropped_batches_count.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "newly_buffered_packets_count",
                    self.newly_buffered_packets_count.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "current_buffered_packets_count",
                    self.current_buffered_packets_count
                        .swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "rebuffered_packets_count",
                    self.rebuffered_packets_count.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
            );
        }
    }
}

/// Stores the stage's thread handle and output receiver.
pub struct BankingStage {
    bank_thread_hdls: Vec<JoinHandle<()>>,
}

#[derive(Debug, Clone)]
pub enum BufferedPacketsDecision {
    Consume(u128),
    Forward,
    ForwardAndHold,
    Hold,
}

impl BankingStage {
    /// Create the stage using `bank`. Exit when `verified_receiver` is dropped.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        verified_receiver: CrossbeamReceiver<Vec<Packets>>,
        verified_vote_receiver: CrossbeamReceiver<Vec<Packets>>,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: ReplayVoteSender,
    ) -> Self {
        Self::new_num_threads(
            cluster_info,
            poh_recorder,
            verified_receiver,
            verified_vote_receiver,
            Self::num_threads(),
            transaction_status_sender,
            gossip_vote_sender,
        )
    }

    fn new_num_threads(
        cluster_info: &Arc<ClusterInfo>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        verified_receiver: CrossbeamReceiver<Vec<Packets>>,
        verified_vote_receiver: CrossbeamReceiver<Vec<Packets>>,
        num_threads: u32,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: ReplayVoteSender,
    ) -> Self {
        let batch_limit = TOTAL_BUFFERED_PACKETS / ((num_threads - 1) as usize * PACKETS_PER_BATCH);
        // Single thread to generate entries from many banks.
        // This thread talks to poh_service and broadcasts the entries once they have been recorded.
        // Once an entry has been recorded, its blockhash is registered with the bank.
        let my_pubkey = cluster_info.id();
        let duplicates = Arc::new(Mutex::new((
            LruCache::new(DEFAULT_LRU_SIZE),
            PacketHasher::default(),
        )));
        // Many banks that process transactions in parallel.
        let bank_thread_hdls: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|i| {
                let (verified_receiver, enable_forwarding) = if i < num_threads - 1 {
                    (verified_receiver.clone(), true)
                } else {
                    // Disable forwarding of vote transactions, as votes are gossiped
                    (verified_vote_receiver.clone(), false)
                };

                let poh_recorder = poh_recorder.clone();
                let cluster_info = cluster_info.clone();
                let mut recv_start = Instant::now();
                let transaction_status_sender = transaction_status_sender.clone();
                let gossip_vote_sender = gossip_vote_sender.clone();
                let duplicates = duplicates.clone();
                Builder::new()
                    .name("solana-banking-stage-tx".to_string())
                    .spawn(move || {
                        thread_mem_usage::datapoint("solana-banking-stage-tx");
                        Self::process_loop(
                            my_pubkey,
                            &verified_receiver,
                            &poh_recorder,
                            &cluster_info,
                            &mut recv_start,
                            enable_forwarding,
                            i,
                            batch_limit,
                            transaction_status_sender,
                            gossip_vote_sender,
                            &duplicates,
                        );
                    })
                    .unwrap()
            })
            .collect();
        Self { bank_thread_hdls }
    }

    fn filter_valid_packets_for_forwarding<'a>(
        all_packets: impl Iterator<Item = &'a PacketsAndOffsets>,
    ) -> Vec<&'a Packet> {
        all_packets
            .filter(|(_p, _indexes, forwarded)| !forwarded)
            .flat_map(|(p, valid_indexes, _forwarded)| {
                valid_indexes.iter().map(move |x| &p.packets[*x])
            })
            .collect()
    }

    fn forward_buffered_packets(
        socket: &std::net::UdpSocket,
        tpu_forwards: &std::net::SocketAddr,
        unprocessed_packets: &UnprocessedPackets,
    ) -> std::io::Result<()> {
        let packets = Self::filter_valid_packets_for_forwarding(unprocessed_packets.iter());
        inc_new_counter_info!("banking_stage-forwarded_packets", packets.len());
        for p in packets {
            socket.send_to(&p.data[..p.meta.size], &tpu_forwards)?;
        }

        Ok(())
    }

    // Returns whether the given `Packets` has any more remaining unprocessed
    // transactions
    fn update_buffered_packets_with_new_unprocessed(
        original_unprocessed_indexes: &mut Vec<usize>,
        new_unprocessed_indexes: Vec<usize>,
    ) -> bool {
        let has_more_unprocessed_transactions =
            Self::packet_has_more_unprocessed_transactions(&new_unprocessed_indexes);
        if has_more_unprocessed_transactions {
            *original_unprocessed_indexes = new_unprocessed_indexes
        };
        has_more_unprocessed_transactions
    }

    pub fn consume_buffered_packets(
        my_pubkey: &Pubkey,
        max_tx_ingestion_ns: u128,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        buffered_packets: &mut UnprocessedPackets,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
        test_fn: Option<impl Fn()>,
        banking_stage_stats: Option<&BankingStageStats>,
        recorder: &TransactionRecorder,
    ) {
        let mut rebuffered_packets_len = 0;
        let mut new_tx_count = 0;
        let buffered_len = buffered_packets.len();
        let mut proc_start = Measure::start("consume_buffered_process");
        let mut reached_end_of_slot = None;
        buffered_packets.retain_mut(|(msgs, ref mut original_unprocessed_indexes, _forwarded)| {
            if let Some((next_leader, bank)) = &reached_end_of_slot {
                // We've hit the end of this slot, no need to perform more processing,
                // just filter the remaining packets for the invalid (e.g. too old) ones
                let new_unprocessed_indexes = Self::filter_unprocessed_packets(
                    &bank,
                    &msgs,
                    &original_unprocessed_indexes,
                    my_pubkey,
                    *next_leader,
                );
                Self::update_buffered_packets_with_new_unprocessed(
                    original_unprocessed_indexes,
                    new_unprocessed_indexes,
                )
            } else {
                let bank_start = poh_recorder.lock().unwrap().bank_start();
                if let Some((bank, bank_creation_time)) = bank_start {
                    let (processed, verified_txs_len, new_unprocessed_indexes) =
                        Self::process_packets_transactions(
                            &bank,
                            &bank_creation_time,
                            &recorder,
                            &msgs,
                            original_unprocessed_indexes.to_owned(),
                            transaction_status_sender.clone(),
                            gossip_vote_sender,
                        );
                    if processed < verified_txs_len
                        || !Bank::should_bank_still_be_processing_txs(
                            &bank_creation_time,
                            max_tx_ingestion_ns,
                        )
                    {
                        reached_end_of_slot =
                            Some((poh_recorder.lock().unwrap().next_slot_leader(), bank));
                    }
                    new_tx_count += processed;
                    // Out of the buffered packets just retried, collect any still unprocessed
                    // transactions in this batch for forwarding
                    rebuffered_packets_len += new_unprocessed_indexes.len();
                    let has_more_unprocessed_transactions =
                        Self::update_buffered_packets_with_new_unprocessed(
                            original_unprocessed_indexes,
                            new_unprocessed_indexes,
                        );
                    if let Some(test_fn) = &test_fn {
                        test_fn();
                    }
                    has_more_unprocessed_transactions
                } else {
                    rebuffered_packets_len += original_unprocessed_indexes.len();
                    // `original_unprocessed_indexes` must have remaining packets to process
                    // if not yet processed.
                    assert!(Self::packet_has_more_unprocessed_transactions(
                        &original_unprocessed_indexes
                    ));
                    true
                }
            }
        });

        proc_start.stop();

        debug!(
            "@{:?} done processing buffered batches: {} time: {:?}ms tx count: {} tx/s: {}",
            timestamp(),
            buffered_len,
            proc_start.as_ms(),
            new_tx_count,
            (new_tx_count as f32) / (proc_start.as_s())
        );

        if let Some(stats) = banking_stage_stats {
            stats
                .rebuffered_packets_count
                .fetch_add(rebuffered_packets_len, Ordering::Relaxed);
            stats
                .consumed_buffered_packets_count
                .fetch_add(new_tx_count, Ordering::Relaxed);
        }
    }

    fn consume_or_forward_packets(
        my_pubkey: &Pubkey,
        leader_pubkey: Option<Pubkey>,
        bank_still_processing_txs: Option<&Arc<Bank>>,
        would_be_leader: bool,
        would_be_leader_shortly: bool,
    ) -> BufferedPacketsDecision {
        leader_pubkey.map_or(
            // If leader is not known, return the buffered packets as is
            BufferedPacketsDecision::Hold,
            // else process the packets
            |x| {
                if let Some(bank) = bank_still_processing_txs {
                    // If the bank is available, this node is the leader
                    BufferedPacketsDecision::Consume(bank.ns_per_slot)
                } else if would_be_leader_shortly {
                    // If the node will be the leader soon, hold the packets for now
                    BufferedPacketsDecision::Hold
                } else if would_be_leader {
                    // Node will be leader within ~20 slots, hold the transactions in
                    // case it is the only node which produces an accepted slot.
                    BufferedPacketsDecision::ForwardAndHold
                } else if x != *my_pubkey {
                    // If the current node is not the leader, forward the buffered packets
                    BufferedPacketsDecision::Forward
                } else {
                    // We don't know the leader. Hold the packets for now
                    BufferedPacketsDecision::Hold
                }
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn process_buffered_packets(
        my_pubkey: &Pubkey,
        socket: &std::net::UdpSocket,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        cluster_info: &ClusterInfo,
        buffered_packets: &mut UnprocessedPackets,
        enable_forwarding: bool,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
        banking_stage_stats: &BankingStageStats,
        recorder: &TransactionRecorder,
    ) -> BufferedPacketsDecision {
        let bank_start;
        let (
            leader_at_slot_offset,
            bank_still_processing_txs,
            would_be_leader,
            would_be_leader_shortly,
        ) = {
            let poh = poh_recorder.lock().unwrap();
            bank_start = poh.bank_start();
            (
                poh.leader_after_n_slots(FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET),
                PohRecorder::get_bank_still_processing_txs(&bank_start),
                poh.would_be_leader(HOLD_TRANSACTIONS_SLOT_OFFSET * DEFAULT_TICKS_PER_SLOT),
                poh.would_be_leader(
                    (FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET - 1) * DEFAULT_TICKS_PER_SLOT,
                ),
            )
        };

        let decision = Self::consume_or_forward_packets(
            my_pubkey,
            leader_at_slot_offset,
            bank_still_processing_txs,
            would_be_leader,
            would_be_leader_shortly,
        );

        match decision {
            BufferedPacketsDecision::Consume(max_tx_ingestion_ns) => {
                Self::consume_buffered_packets(
                    my_pubkey,
                    max_tx_ingestion_ns,
                    poh_recorder,
                    buffered_packets,
                    transaction_status_sender,
                    gossip_vote_sender,
                    None::<Box<dyn Fn()>>,
                    Some(banking_stage_stats),
                    recorder,
                );
            }
            BufferedPacketsDecision::Forward => {
                Self::handle_forwarding(
                    enable_forwarding,
                    cluster_info,
                    buffered_packets,
                    poh_recorder,
                    socket,
                    false,
                );
            }
            BufferedPacketsDecision::ForwardAndHold => {
                Self::handle_forwarding(
                    enable_forwarding,
                    cluster_info,
                    buffered_packets,
                    poh_recorder,
                    socket,
                    true,
                );
            }
            _ => (),
        }
        decision
    }

    fn handle_forwarding(
        enable_forwarding: bool,
        cluster_info: &ClusterInfo,
        buffered_packets: &mut UnprocessedPackets,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        socket: &UdpSocket,
        hold: bool,
    ) {
        if enable_forwarding {
            let next_leader = poh_recorder
                .lock()
                .unwrap()
                .leader_after_n_slots(FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET);
            next_leader.map_or((), |leader_pubkey| {
                let leader_addr = {
                    cluster_info.lookup_contact_info(&leader_pubkey, |leader| leader.tpu_forwards)
                };

                leader_addr.map_or((), |leader_addr| {
                    let _ =
                        Self::forward_buffered_packets(&socket, &leader_addr, &buffered_packets);
                    if hold {
                        buffered_packets.retain(|b| b.1.is_empty());
                        for b in buffered_packets.iter_mut() {
                            b.2 = true;
                        }
                    } else {
                        buffered_packets.clear();
                    }
                })
            })
        } else {
            buffered_packets.clear();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_loop(
        my_pubkey: Pubkey,
        verified_receiver: &CrossbeamReceiver<Vec<Packets>>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
        cluster_info: &ClusterInfo,
        recv_start: &mut Instant,
        enable_forwarding: bool,
        id: u32,
        batch_limit: usize,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: ReplayVoteSender,
        duplicates: &Arc<Mutex<(LruCache<u64, ()>, PacketHasher)>>,
    ) {
        let recorder = poh_recorder.lock().unwrap().recorder();
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let mut buffered_packets = VecDeque::with_capacity(batch_limit);
        let banking_stage_stats = BankingStageStats::new(id);
        loop {
            while !buffered_packets.is_empty() {
                let decision = Self::process_buffered_packets(
                    &my_pubkey,
                    &socket,
                    &poh_recorder,
                    cluster_info,
                    &mut buffered_packets,
                    enable_forwarding,
                    transaction_status_sender.clone(),
                    &gossip_vote_sender,
                    &banking_stage_stats,
                    &recorder,
                );
                if matches!(decision, BufferedPacketsDecision::Hold)
                    || matches!(decision, BufferedPacketsDecision::ForwardAndHold)
                {
                    // If we are waiting on a new bank,
                    // check the receiver for more transactions/for exiting
                    break;
                }
            }

            let recv_timeout = if !buffered_packets.is_empty() {
                // If packets are buffered, let's wait for less time on recv from the channel.
                // This helps detect the next leader faster, and processing the buffered
                // packets quickly
                Duration::from_millis(10)
            } else {
                // Default wait time
                Duration::from_millis(100)
            };

            match Self::process_packets(
                &my_pubkey,
                &verified_receiver,
                &poh_recorder,
                recv_start,
                recv_timeout,
                id,
                batch_limit,
                transaction_status_sender.clone(),
                &gossip_vote_sender,
                &mut buffered_packets,
                &banking_stage_stats,
                duplicates,
                &recorder,
            ) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => break,
            }

            banking_stage_stats.report(1000);
        }
    }

    pub fn num_threads() -> u32 {
        const MIN_THREADS_VOTES: u32 = 1;
        const MIN_THREADS_BANKING: u32 = 1;
        cmp::max(
            env::var("SOLANA_BANKING_THREADS")
                .map(|x| x.parse().unwrap_or(NUM_THREADS))
                .unwrap_or(NUM_THREADS),
            MIN_THREADS_VOTES + MIN_THREADS_BANKING,
        )
    }

    /// Convert the transactions from a blob of binary data to a vector of transactions
    fn deserialize_transactions(p: &Packets) -> Vec<Option<Transaction>> {
        p.packets
            .iter()
            .map(|x| limited_deserialize(&x.data[0..x.meta.size]).ok())
            .collect()
    }

    #[allow(clippy::match_wild_err_arm)]
    fn record_transactions(
        bank_slot: Slot,
        txs: &[Transaction],
        results: &[TransactionExecutionResult],
        recorder: &TransactionRecorder,
    ) -> (Result<usize, PohRecorderError>, Vec<usize>) {
        let mut processed_generation = Measure::start("record::process_generation");
        let (processed_transactions, processed_transactions_indexes): (Vec<_>, Vec<_>) = results
            .iter()
            .zip(txs.iter())
            .enumerate()
            .filter_map(|(i, ((r, _h), x))| {
                if Bank::can_commit(r) {
                    Some((x.clone(), i))
                } else {
                    None
                }
            })
            .unzip();

        processed_generation.stop();
        let num_to_commit = processed_transactions.len();
        debug!("num_to_commit: {} ", num_to_commit);
        // unlock all the accounts with errors which are filtered by the above `filter_map`
        if !processed_transactions.is_empty() {
            inc_new_counter_info!("banking_stage-record_count", 1);
            inc_new_counter_info!("banking_stage-record_transactions", num_to_commit);

            let mut hash_time = Measure::start("record::hash");
            let hash = hash_transactions(&processed_transactions[..]);
            hash_time.stop();

            let mut poh_record = Measure::start("record::poh_record");
            // record and unlock will unlock all the successful transactions
            let res = recorder.record(bank_slot, hash, processed_transactions);
            match res {
                Ok(()) => (),
                Err(PohRecorderError::MaxHeightReached) => {
                    inc_new_counter_info!("banking_stage-max_height_reached", 1);
                    inc_new_counter_info!(
                        "banking_stage-max_height_reached_num_to_commit",
                        num_to_commit
                    );
                    // If record errors, add all the committable transactions (the ones
                    // we just attempted to record) as retryable
                    return (
                        Err(PohRecorderError::MaxHeightReached),
                        processed_transactions_indexes,
                    );
                }
                Err(e) => panic!("Poh recorder returned unexpected error: {:?}", e),
            }
            poh_record.stop();
        }
        (Ok(num_to_commit), vec![])
    }

    fn process_and_record_transactions_locked(
        bank: &Arc<Bank>,
        poh: &TransactionRecorder,
        batch: &TransactionBatch,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
    ) -> (Result<usize, PohRecorderError>, Vec<usize>) {
        let mut load_execute_time = Measure::start("load_execute_time");
        // Use a shorter maximum age when adding transactions into the pipeline.  This will reduce
        // the likelihood of any single thread getting starved and processing old ids.
        // TODO: Banking stage threads should be prioritized to complete faster then this queue
        // expires.
        let txs = batch.transactions();
        let pre_balances = if transaction_status_sender.is_some() {
            bank.collect_balances(batch)
        } else {
            vec![]
        };

        let mut mint_decimals: HashMap<Pubkey, u8> = HashMap::new();

        let pre_token_balances = if transaction_status_sender.is_some() {
            collect_token_balances(&bank, &batch, &mut mint_decimals)
        } else {
            vec![]
        };

        let mut execute_timings = ExecuteTimings::default();

        let (
            mut loaded_accounts,
            results,
            inner_instructions,
            transaction_logs,
            mut retryable_txs,
            tx_count,
            signature_count,
        ) = bank.load_and_execute_transactions(
            batch,
            MAX_PROCESSING_AGE,
            transaction_status_sender.is_some(),
            transaction_status_sender.is_some(),
            &mut execute_timings,
        );
        load_execute_time.stop();

        let freeze_lock = bank.freeze_lock();

        let mut record_time = Measure::start("record_time");
        let (num_to_commit, retryable_record_txs) =
            Self::record_transactions(bank.slot(), txs, &results, poh);
        inc_new_counter_info!(
            "banking_stage-record_transactions_num_to_commit",
            *num_to_commit.as_ref().unwrap_or(&0)
        );
        inc_new_counter_info!(
            "banking_stage-record_transactions_retryable_record_txs",
            retryable_record_txs.len()
        );
        retryable_txs.extend(retryable_record_txs);
        if num_to_commit.is_err() {
            return (num_to_commit, retryable_txs);
        }
        record_time.stop();

        let mut commit_time = Measure::start("commit_time");

        let num_to_commit = num_to_commit.unwrap();

        if num_to_commit != 0 {
            let tx_results = bank.commit_transactions(
                txs,
                &mut loaded_accounts,
                &results,
                tx_count,
                signature_count,
                &mut execute_timings,
            );

            bank_utils::find_and_send_votes(txs, &tx_results, Some(gossip_vote_sender));
            if let Some(transaction_status_sender) = transaction_status_sender {
                let post_balances = bank.collect_balances(batch);
                let post_token_balances = collect_token_balances(&bank, &batch, &mut mint_decimals);
                transaction_status_sender.send_transaction_status_batch(
                    bank.clone(),
                    batch.transactions(),
                    tx_results.execution_results,
                    TransactionBalancesSet::new(pre_balances, post_balances),
                    TransactionTokenBalancesSet::new(pre_token_balances, post_token_balances),
                    inner_instructions,
                    transaction_logs,
                );
            }
        }
        commit_time.stop();

        drop(freeze_lock);

        debug!(
            "bank: {} process_and_record_locked: {}us record: {}us commit: {}us txs_len: {}",
            bank.slot(),
            load_execute_time.as_us(),
            record_time.as_us(),
            commit_time.as_us(),
            txs.len(),
        );

        debug!(
            "process_and_record_transactions_locked: {:?}",
            execute_timings
        );

        (Ok(num_to_commit), retryable_txs)
    }

    pub fn process_and_record_transactions(
        bank: &Arc<Bank>,
        txs: &[Transaction],
        poh: &TransactionRecorder,
        chunk_offset: usize,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
    ) -> (Result<usize, PohRecorderError>, Vec<usize>) {
        let mut lock_time = Measure::start("lock_time");
        // Once accounts are locked, other threads cannot encode transactions that will modify the
        // same account state
        let batch = bank.prepare_batch(txs);
        lock_time.stop();

        let (result, mut retryable_txs) = Self::process_and_record_transactions_locked(
            bank,
            poh,
            &batch,
            transaction_status_sender,
            gossip_vote_sender,
        );
        retryable_txs.iter_mut().for_each(|x| *x += chunk_offset);

        let mut unlock_time = Measure::start("unlock_time");
        // Once the accounts are new transactions can enter the pipeline to process them
        drop(batch);
        unlock_time.stop();

        debug!(
            "bank: {} lock: {}us unlock: {}us txs_len: {}",
            bank.slot(),
            lock_time.as_us(),
            unlock_time.as_us(),
            txs.len(),
        );

        (result, retryable_txs)
    }

    /// Sends transactions to the bank.
    ///
    /// Returns the number of transactions successfully processed by the bank, which may be less
    /// than the total number if max PoH height was reached and the bank halted
    fn process_transactions(
        bank: &Arc<Bank>,
        bank_creation_time: &Instant,
        transactions: &[Transaction],
        poh: &TransactionRecorder,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
    ) -> (usize, Vec<usize>) {
        let mut chunk_start = 0;
        let mut unprocessed_txs = vec![];
        while chunk_start != transactions.len() {
            let chunk_end = std::cmp::min(
                transactions.len(),
                chunk_start + MAX_NUM_TRANSACTIONS_PER_BATCH,
            );

            let (result, retryable_txs_in_chunk) = Self::process_and_record_transactions(
                bank,
                &transactions[chunk_start..chunk_end],
                poh,
                chunk_start,
                transaction_status_sender.clone(),
                gossip_vote_sender,
            );
            trace!("process_transactions result: {:?}", result);

            // Add the retryable txs (transactions that errored in a way that warrants a retry)
            // to the list of unprocessed txs.
            unprocessed_txs.extend_from_slice(&retryable_txs_in_chunk);

            // If `bank_creation_time` is None, it's a test so ignore the option so
            // allow processing
            let should_bank_still_be_processing_txs =
                Bank::should_bank_still_be_processing_txs(bank_creation_time, bank.ns_per_slot);
            match (result, should_bank_still_be_processing_txs) {
                (Err(PohRecorderError::MaxHeightReached), _) | (_, false) => {
                    info!(
                        "process transactions: max height reached slot: {} height: {}",
                        bank.slot(),
                        bank.tick_height()
                    );
                    // process_and_record_transactions has returned all retryable errors in
                    // transactions[chunk_start..chunk_end], so we just need to push the remaining
                    // transactions into the unprocessed queue.
                    unprocessed_txs.extend(chunk_end..transactions.len());
                    break;
                }
                _ => (),
            }
            // Don't exit early on any other type of error, continue processing...
            chunk_start = chunk_end;
        }

        (chunk_start, unprocessed_txs)
    }

    // This function returns a vector of transactions that are not None. It also returns a vector
    // with position of the transaction in the input list
    fn filter_transaction_indexes(
        transactions: Vec<Option<Transaction>>,
        indexes: &[usize],
    ) -> (Vec<Transaction>, Vec<usize>) {
        transactions
            .into_iter()
            .zip(indexes)
            .filter_map(|(tx, index)| tx.map(|tx| (tx, index)))
            .unzip()
    }

    // This function creates a filter of transaction results with Ok() for every pending
    // transaction. The non-pending transactions are marked with TransactionError
    fn prepare_filter_for_pending_transactions(
        transactions: &[Transaction],
        pending_tx_indexes: &[usize],
    ) -> Vec<transaction::Result<()>> {
        let mut mask = vec![Err(TransactionError::BlockhashNotFound); transactions.len()];
        pending_tx_indexes.iter().for_each(|x| mask[*x] = Ok(()));
        mask
    }

    // This function returns a vector containing index of all valid transactions. A valid
    // transaction has result Ok() as the value
    fn filter_valid_transaction_indexes(
        valid_txs: &[TransactionCheckResult],
        transaction_indexes: &[usize],
    ) -> Vec<usize> {
        let valid_transactions = valid_txs
            .iter()
            .enumerate()
            .filter_map(|(index, (x, _h))| if x.is_ok() { Some(index) } else { None })
            .collect_vec();

        valid_transactions
            .iter()
            .map(|x| transaction_indexes[*x])
            .collect()
    }

    // This function deserializes packets into transactions and returns non-None transactions
    fn transactions_from_packets(
        msgs: &Packets,
        transaction_indexes: &[usize],
        secp256k1_program_enabled: bool,
    ) -> (Vec<Transaction>, Vec<usize>) {
        let packets = Packets::new(
            transaction_indexes
                .iter()
                .map(|x| msgs.packets[*x].to_owned())
                .collect_vec(),
        );

        let transactions = Self::deserialize_transactions(&packets);
        let maybe_secp_verified_transactions: Vec<_> = if secp256k1_program_enabled {
            transactions
                .into_iter()
                .map(|tx| {
                    if let Some(tx) = tx {
                        if tx.verify_precompiles().is_ok() {
                            Some(tx)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            transactions
        };

        Self::filter_transaction_indexes(maybe_secp_verified_transactions, &transaction_indexes)
    }

    /// This function filters pending packets that are still valid
    /// # Arguments
    /// * `transactions` - a batch of transactions deserialized from packets
    /// * `transaction_to_packet_indexes` - maps each transaction to a packet index
    /// * `pending_indexes` - identifies which indexes in the `transactions` list are still pending
    fn filter_pending_packets_from_pending_txs(
        bank: &Arc<Bank>,
        transactions: &[Transaction],
        transaction_to_packet_indexes: &[usize],
        pending_indexes: &[usize],
    ) -> Vec<usize> {
        let filter = Self::prepare_filter_for_pending_transactions(transactions, pending_indexes);

        let mut error_counters = ErrorCounters::default();
        // The following code also checks if the blockhash for a transaction is too old
        // The check accounts for
        //  1. Transaction forwarding delay
        //  2. The slot at which the next leader will actually process the transaction
        // Drop the transaction if it will expire by the time the next node receives and processes it
        let api = perf_libs::api();
        let max_tx_fwd_delay = if api.is_none() {
            MAX_TRANSACTION_FORWARDING_DELAY
        } else {
            MAX_TRANSACTION_FORWARDING_DELAY_GPU
        };
        let result = bank.check_transactions(
            transactions,
            &filter,
            (MAX_PROCESSING_AGE)
                .saturating_sub(max_tx_fwd_delay)
                .saturating_sub(FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET as usize),
            &mut error_counters,
        );

        Self::filter_valid_transaction_indexes(&result, transaction_to_packet_indexes)
    }

    fn process_packets_transactions(
        bank: &Arc<Bank>,
        bank_creation_time: &Instant,
        poh: &TransactionRecorder,
        msgs: &Packets,
        packet_indexes: Vec<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
    ) -> (usize, usize, Vec<usize>) {
        let (transactions, transaction_to_packet_indexes) = Self::transactions_from_packets(
            msgs,
            &packet_indexes,
            bank.secp256k1_program_enabled(),
        );
        debug!(
            "bank: {} filtered transactions {}",
            bank.slot(),
            transactions.len()
        );

        let tx_len = transactions.len();

        let (processed, unprocessed_tx_indexes) = Self::process_transactions(
            bank,
            bank_creation_time,
            &transactions,
            poh,
            transaction_status_sender,
            gossip_vote_sender,
        );

        let unprocessed_tx_count = unprocessed_tx_indexes.len();

        let filtered_unprocessed_packet_indexes = Self::filter_pending_packets_from_pending_txs(
            bank,
            &transactions,
            &transaction_to_packet_indexes,
            &unprocessed_tx_indexes,
        );
        inc_new_counter_info!(
            "banking_stage-dropped_tx_before_forwarding",
            unprocessed_tx_count.saturating_sub(filtered_unprocessed_packet_indexes.len())
        );

        (processed, tx_len, filtered_unprocessed_packet_indexes)
    }

    fn filter_unprocessed_packets(
        bank: &Arc<Bank>,
        msgs: &Packets,
        transaction_indexes: &[usize],
        my_pubkey: &Pubkey,
        next_leader: Option<Pubkey>,
    ) -> Vec<usize> {
        // Check if we are the next leader. If so, let's not filter the packets
        // as we'll filter it again while processing the packets.
        // Filtering helps if we were going to forward the packets to some other node
        if let Some(leader) = next_leader {
            if leader == *my_pubkey {
                return transaction_indexes.to_vec();
            }
        }

        let (transactions, transaction_to_packet_indexes) = Self::transactions_from_packets(
            msgs,
            &transaction_indexes,
            bank.secp256k1_program_enabled(),
        );

        let tx_count = transaction_to_packet_indexes.len();

        let unprocessed_tx_indexes = (0..transactions.len()).collect_vec();
        let filtered_unprocessed_packet_indexes = Self::filter_pending_packets_from_pending_txs(
            bank,
            &transactions,
            &transaction_to_packet_indexes,
            &unprocessed_tx_indexes,
        );

        inc_new_counter_info!(
            "banking_stage-dropped_tx_before_forwarding",
            tx_count.saturating_sub(filtered_unprocessed_packet_indexes.len())
        );

        filtered_unprocessed_packet_indexes
    }

    fn generate_packet_indexes(vers: &PinnedVec<Packet>) -> Vec<usize> {
        vers.iter()
            .enumerate()
            .filter_map(
                |(index, ver)| {
                    if !ver.meta.discard {
                        Some(index)
                    } else {
                        None
                    }
                },
            )
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    /// Process the incoming packets
    pub fn process_packets(
        my_pubkey: &Pubkey,
        verified_receiver: &CrossbeamReceiver<Vec<Packets>>,
        poh: &Arc<Mutex<PohRecorder>>,
        recv_start: &mut Instant,
        recv_timeout: Duration,
        id: u32,
        batch_limit: usize,
        transaction_status_sender: Option<TransactionStatusSender>,
        gossip_vote_sender: &ReplayVoteSender,
        buffered_packets: &mut UnprocessedPackets,
        banking_stage_stats: &BankingStageStats,
        duplicates: &Arc<Mutex<(LruCache<u64, ()>, PacketHasher)>>,
        recorder: &TransactionRecorder,
    ) -> Result<(), RecvTimeoutError> {
        let mut recv_time = Measure::start("process_packets_recv");
        let mms = verified_receiver.recv_timeout(recv_timeout)?;
        recv_time.stop();

        let mms_len = mms.len();
        let count: usize = mms.iter().map(|x| x.packets.len()).sum();
        debug!(
            "@{:?} process start stalled for: {:?}ms txs: {} id: {}",
            timestamp(),
            duration_as_ms(&recv_start.elapsed()),
            count,
            id,
        );
        inc_new_counter_debug!("banking_stage-transactions_received", count);
        let mut proc_start = Measure::start("process_packets_transactions_process");
        let mut new_tx_count = 0;

        let mut mms_iter = mms.into_iter();
        let mut dropped_batches_count = 0;
        let mut newly_buffered_packets_count = 0;
        while let Some(msgs) = mms_iter.next() {
            let packet_indexes = Self::generate_packet_indexes(&msgs.packets);
            let bank_start = poh.lock().unwrap().bank_start();
            if PohRecorder::get_bank_still_processing_txs(&bank_start).is_none() {
                Self::push_unprocessed(
                    buffered_packets,
                    msgs,
                    packet_indexes,
                    &mut dropped_batches_count,
                    &mut newly_buffered_packets_count,
                    batch_limit,
                    duplicates,
                );
                continue;
            }
            let (bank, bank_creation_time) = bank_start.unwrap();

            let (processed, verified_txs_len, unprocessed_indexes) =
                Self::process_packets_transactions(
                    &bank,
                    &bank_creation_time,
                    recorder,
                    &msgs,
                    packet_indexes,
                    transaction_status_sender.clone(),
                    gossip_vote_sender,
                );

            new_tx_count += processed;

            // Collect any unprocessed transactions in this batch for forwarding
            Self::push_unprocessed(
                buffered_packets,
                msgs,
                unprocessed_indexes,
                &mut dropped_batches_count,
                &mut newly_buffered_packets_count,
                batch_limit,
                duplicates,
            );

            // If there were retryable transactions, add the unexpired ones to the buffered queue
            if processed < verified_txs_len {
                let next_leader = poh.lock().unwrap().next_slot_leader();
                // Walk thru rest of the transactions and filter out the invalid (e.g. too old) ones
                #[allow(clippy::while_let_on_iterator)]
                while let Some(msgs) = mms_iter.next() {
                    let packet_indexes = Self::generate_packet_indexes(&msgs.packets);
                    let unprocessed_indexes = Self::filter_unprocessed_packets(
                        &bank,
                        &msgs,
                        &packet_indexes,
                        &my_pubkey,
                        next_leader,
                    );
                    Self::push_unprocessed(
                        buffered_packets,
                        msgs,
                        unprocessed_indexes,
                        &mut dropped_batches_count,
                        &mut newly_buffered_packets_count,
                        batch_limit,
                        duplicates,
                    );
                }
            }
        }

        proc_start.stop();

        inc_new_counter_debug!("banking_stage-time_ms", proc_start.as_ms() as usize);
        debug!(
            "@{:?} done processing transaction batches: {} time: {:?}ms tx count: {} tx/s: {} total count: {} id: {}",
            timestamp(),
            mms_len,
            proc_start.as_ms(),
            new_tx_count,
            (new_tx_count as f32) / (proc_start.as_s()),
            count,
            id,
        );
        banking_stage_stats
            .process_packets_count
            .fetch_add(count, Ordering::Relaxed);
        banking_stage_stats
            .new_tx_count
            .fetch_add(new_tx_count, Ordering::Relaxed);
        banking_stage_stats
            .dropped_batches_count
            .fetch_add(dropped_batches_count, Ordering::Relaxed);
        banking_stage_stats
            .newly_buffered_packets_count
            .fetch_add(newly_buffered_packets_count, Ordering::Relaxed);
        banking_stage_stats
            .current_buffered_packets_count
            .swap(buffered_packets.len(), Ordering::Relaxed);
        *recv_start = Instant::now();
        Ok(())
    }

    fn push_unprocessed(
        unprocessed_packets: &mut UnprocessedPackets,
        packets: Packets,
        mut packet_indexes: Vec<usize>,
        dropped_batches_count: &mut usize,
        newly_buffered_packets_count: &mut usize,
        batch_limit: usize,
        duplicates: &Arc<Mutex<(LruCache<u64, ()>, PacketHasher)>>,
    ) {
        {
            let mut duplicates = duplicates.lock().unwrap();
            let (cache, hasher) = duplicates.deref_mut();
            packet_indexes.retain(|i| {
                let packet_hash = hasher.hash_packet(&packets.packets[*i]);
                match cache.get_mut(&packet_hash) {
                    Some(_hash) => false,
                    None => {
                        cache.put(packet_hash, ());
                        true
                    }
                }
            });
        }
        if Self::packet_has_more_unprocessed_transactions(&packet_indexes) {
            if unprocessed_packets.len() >= batch_limit {
                *dropped_batches_count += 1;
                unprocessed_packets.pop_front();
            }
            *newly_buffered_packets_count += packet_indexes.len();
            unprocessed_packets.push_back((packets, packet_indexes, false));
        }
    }

    fn packet_has_more_unprocessed_transactions(packet_indexes: &[usize]) -> bool {
        !packet_indexes.is_empty()
    }

    pub fn join(self) -> thread::Result<()> {
        for bank_thread_hdl in self.bank_thread_hdls {
            bank_thread_hdl.join()?;
        }
        Ok(())
    }
}

pub fn create_test_recorder(
    bank: &Arc<Bank>,
    blockstore: &Arc<Blockstore>,
    poh_config: Option<PohConfig>,
) -> (
    Arc<AtomicBool>,
    Arc<Mutex<PohRecorder>>,
    PohService,
    Receiver<WorkingBankEntry>,
) {
    let exit = Arc::new(AtomicBool::new(false));
    let poh_config = Arc::new(poh_config.unwrap_or_default());
    let (mut poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
        bank.tick_height(),
        bank.last_blockhash(),
        bank.slot(),
        Some((4, 4)),
        bank.ticks_per_slot(),
        &Pubkey::default(),
        blockstore,
        &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
        &poh_config,
    );
    poh_recorder.set_bank(&bank);

    let poh_recorder = Arc::new(Mutex::new(poh_recorder));
    let poh_service = PohService::new(
        poh_recorder.clone(),
        &poh_config,
        &exit,
        bank.ticks_per_slot(),
        poh_service::DEFAULT_PINNED_CPU_CORE,
        poh_service::DEFAULT_HASHES_PER_BATCH,
        record_receiver,
    );

    (exit, poh_recorder, poh_service, entry_receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cluster_info::Node, poh_recorder::Record, poh_recorder::WorkingBank,
        transaction_status_service::TransactionStatusService,
    };
    use crossbeam_channel::unbounded;
    use itertools::Itertools;
    use solana_ledger::{
        blockstore::entries_to_test_shreds,
        entry::{next_entry, Entry, EntrySlice},
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        get_tmp_ledger_path,
    };
    use solana_perf::packet::to_packets_chunked;
    use solana_sdk::{
        instruction::InstructionError,
        signature::{Keypair, Signer},
        system_instruction::SystemError,
        system_transaction,
        transaction::TransactionError,
    };
    use solana_transaction_status::TransactionWithStatusMeta;
    use std::{
        net::SocketAddr,
        path::Path,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc::Receiver,
        },
        thread::sleep,
    };

    #[test]
    fn test_banking_stage_shutdown1() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let bank = Arc::new(Bank::new(&genesis_config));
        let (verified_sender, verified_receiver) = unbounded();
        let (vote_sender, vote_receiver) = unbounded();
        let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Arc::new(
                Blockstore::open(&ledger_path)
                    .expect("Expected to be able to open database ledger"),
            );
            let (exit, poh_recorder, poh_service, _entry_receiever) =
                create_test_recorder(&bank, &blockstore, None);
            let cluster_info = ClusterInfo::new_with_invalid_keypair(Node::new_localhost().info);
            let cluster_info = Arc::new(cluster_info);
            let banking_stage = BankingStage::new(
                &cluster_info,
                &poh_recorder,
                verified_receiver,
                vote_receiver,
                None,
                gossip_vote_sender,
            );
            drop(verified_sender);
            drop(vote_sender);
            exit.store(true, Ordering::Relaxed);
            banking_stage.join().unwrap();
            poh_service.join().unwrap();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_banking_stage_tick() {
        solana_logger::setup();
        let GenesisConfigInfo {
            mut genesis_config, ..
        } = create_genesis_config(2);
        genesis_config.ticks_per_slot = 4;
        let num_extra_ticks = 2;
        let bank = Arc::new(Bank::new(&genesis_config));
        let start_hash = bank.last_blockhash();
        let (verified_sender, verified_receiver) = unbounded();
        let (vote_sender, vote_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Arc::new(
                Blockstore::open(&ledger_path)
                    .expect("Expected to be able to open database ledger"),
            );
            let poh_config = PohConfig {
                target_tick_count: Some(bank.max_tick_height() + num_extra_ticks),
                ..PohConfig::default()
            };
            let (exit, poh_recorder, poh_service, entry_receiver) =
                create_test_recorder(&bank, &blockstore, Some(poh_config));
            let cluster_info = ClusterInfo::new_with_invalid_keypair(Node::new_localhost().info);
            let cluster_info = Arc::new(cluster_info);
            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                &cluster_info,
                &poh_recorder,
                verified_receiver,
                vote_receiver,
                None,
                gossip_vote_sender,
            );
            trace!("sending bank");
            drop(verified_sender);
            drop(vote_sender);
            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
            drop(poh_recorder);

            trace!("getting entries");
            let entries: Vec<_> = entry_receiver
                .iter()
                .map(|(_bank, (entry, _tick_height))| entry)
                .collect();
            trace!("done");
            assert_eq!(entries.len(), genesis_config.ticks_per_slot as usize);
            assert_eq!(entries.verify(&start_hash), true);
            assert_eq!(entries[entries.len() - 1].hash, bank.last_blockhash());
            banking_stage.join().unwrap();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    pub fn convert_from_old_verified(mut with_vers: Vec<(Packets, Vec<u8>)>) -> Vec<Packets> {
        with_vers.iter_mut().for_each(|(b, v)| {
            b.packets
                .iter_mut()
                .zip(v)
                .for_each(|(p, f)| p.meta.discard = *f == 0)
        });
        with_vers.into_iter().map(|(b, _)| b).collect()
    }

    #[test]
    fn test_banking_stage_entries_only() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10);
        let bank = Arc::new(Bank::new_no_wallclock_throttle(&genesis_config));
        let start_hash = bank.last_blockhash();
        let (verified_sender, verified_receiver) = unbounded();
        let (vote_sender, vote_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Arc::new(
                Blockstore::open(&ledger_path)
                    .expect("Expected to be able to open database ledger"),
            );
            let poh_config = PohConfig {
                // limit tick count to avoid clearing working_bank at PohRecord then
                // PohRecorderError(MaxHeightReached) at BankingStage
                target_tick_count: Some(bank.max_tick_height() - 1),
                ..PohConfig::default()
            };
            let (exit, poh_recorder, poh_service, entry_receiver) =
                create_test_recorder(&bank, &blockstore, Some(poh_config));
            let cluster_info = ClusterInfo::new_with_invalid_keypair(Node::new_localhost().info);
            let cluster_info = Arc::new(cluster_info);
            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let banking_stage = BankingStage::new(
                &cluster_info,
                &poh_recorder,
                verified_receiver,
                vote_receiver,
                None,
                gossip_vote_sender,
            );

            // fund another account so we can send 2 good transactions in a single batch.
            let keypair = Keypair::new();
            let fund_tx =
                system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 2, start_hash);
            bank.process_transaction(&fund_tx).unwrap();

            // good tx
            let to = solana_sdk::pubkey::new_rand();
            let tx = system_transaction::transfer(&mint_keypair, &to, 1, start_hash);

            // good tx, but no verify
            let to2 = solana_sdk::pubkey::new_rand();
            let tx_no_ver = system_transaction::transfer(&keypair, &to2, 2, start_hash);

            // bad tx, AccountNotFound
            let keypair = Keypair::new();
            let to3 = solana_sdk::pubkey::new_rand();
            let tx_anf = system_transaction::transfer(&keypair, &to3, 1, start_hash);

            // send 'em over
            let packets = to_packets_chunked(&[tx_no_ver, tx_anf, tx], 3);

            // glad they all fit
            assert_eq!(packets.len(), 1);

            let packets = packets
                .into_iter()
                .map(|packets| (packets, vec![0u8, 1u8, 1u8]))
                .collect();
            let packets = convert_from_old_verified(packets);
            verified_sender // no_ver, anf, tx
                .send(packets)
                .unwrap();

            drop(verified_sender);
            drop(vote_sender);
            // wait until banking_stage to finish up all packets
            banking_stage.join().unwrap();

            exit.store(true, Ordering::Relaxed);
            poh_service.join().unwrap();
            drop(poh_recorder);

            let mut blockhash = start_hash;
            let bank = Arc::new(Bank::new_no_wallclock_throttle(&genesis_config));
            bank.process_transaction(&fund_tx).unwrap();
            //receive entries + ticks
            loop {
                let entries: Vec<Entry> = entry_receiver
                    .iter()
                    .map(|(_bank, (entry, _tick_height))| entry)
                    .collect();

                assert_eq!(entries.verify(&blockhash), true);
                if !entries.is_empty() {
                    blockhash = entries.last().unwrap().hash;
                    for entry in entries {
                        bank.process_transactions(&entry.transactions)
                            .iter()
                            .for_each(|x| assert_eq!(*x, Ok(())));
                    }
                }

                if bank.get_balance(&to) == 1 {
                    break;
                }

                sleep(Duration::from_millis(200));
            }

            assert_eq!(bank.get_balance(&to), 1);
            assert_eq!(bank.get_balance(&to2), 0);

            drop(entry_receiver);
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_banking_stage_entryfication() {
        solana_logger::setup();
        // In this attack we'll demonstrate that a verifier can interpret the ledger
        // differently if either the server doesn't signal the ledger to add an
        // Entry OR if the verifier tries to parallelize across multiple Entries.
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(2);
        let (verified_sender, verified_receiver) = unbounded();

        // Process a batch that includes a transaction that receives two lamports.
        let alice = Keypair::new();
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 2, genesis_config.hash());

        let packets = to_packets_chunked(&[tx], 1);
        let packets = packets
            .into_iter()
            .map(|packets| (packets, vec![1u8]))
            .collect();
        let packets = convert_from_old_verified(packets);
        verified_sender.send(packets).unwrap();

        // Process a second batch that uses the same from account, so conflicts with above TX
        let tx =
            system_transaction::transfer(&mint_keypair, &alice.pubkey(), 1, genesis_config.hash());
        let packets = to_packets_chunked(&[tx], 1);
        let packets = packets
            .into_iter()
            .map(|packets| (packets, vec![1u8]))
            .collect();
        let packets = convert_from_old_verified(packets);
        verified_sender.send(packets).unwrap();

        let (vote_sender, vote_receiver) = unbounded();
        let ledger_path = get_tmp_ledger_path!();
        {
            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let entry_receiver = {
                // start a banking_stage to eat verified receiver
                let bank = Arc::new(Bank::new_no_wallclock_throttle(&genesis_config));
                let blockstore = Arc::new(
                    Blockstore::open(&ledger_path)
                        .expect("Expected to be able to open database ledger"),
                );
                let poh_config = PohConfig {
                    // limit tick count to avoid clearing working_bank at
                    // PohRecord then PohRecorderError(MaxHeightReached) at BankingStage
                    target_tick_count: Some(bank.max_tick_height() - 1),
                    ..PohConfig::default()
                };
                let (exit, poh_recorder, poh_service, entry_receiver) =
                    create_test_recorder(&bank, &blockstore, Some(poh_config));
                let cluster_info =
                    ClusterInfo::new_with_invalid_keypair(Node::new_localhost().info);
                let cluster_info = Arc::new(cluster_info);
                let _banking_stage = BankingStage::new_num_threads(
                    &cluster_info,
                    &poh_recorder,
                    verified_receiver,
                    vote_receiver,
                    2,
                    None,
                    gossip_vote_sender,
                );

                // wait for banking_stage to eat the packets
                while bank.get_balance(&alice.pubkey()) < 2 {
                    sleep(Duration::from_millis(100));
                }
                exit.store(true, Ordering::Relaxed);
                poh_service.join().unwrap();
                entry_receiver
            };
            drop(verified_sender);
            drop(vote_sender);

            // consume the entire entry_receiver, feed it into a new bank
            // check that the balance is what we expect.
            let entries: Vec<_> = entry_receiver
                .iter()
                .map(|(_bank, (entry, _tick_height))| entry)
                .collect();

            let bank = Bank::new_no_wallclock_throttle(&genesis_config);
            for entry in &entries {
                bank.process_transactions(&entry.transactions)
                    .iter()
                    .for_each(|x| assert_eq!(*x, Ok(())));
            }

            // Assert the user holds two lamports, not three. If the stage only outputs one
            // entry, then the second transaction will be rejected, because it drives
            // the account balance below zero before the credit is added.
            assert_eq!(bank.get_balance(&alice.pubkey()), 2);
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_bank_record_transactions() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));
        let start = Arc::new(Instant::now());
        let working_bank = WorkingBank {
            bank: bank.clone(),
            start,
            min_tick_height: bank.tick_height(),
            max_tick_height: std::u64::MAX,
        };
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&ledger_path)
                .expect("Expected to be able to open database ledger");
            let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
                // TODO use record_receiver
                bank.tick_height(),
                bank.last_blockhash(),
                bank.slot(),
                None,
                bank.ticks_per_slot(),
                &Pubkey::default(),
                &Arc::new(blockstore),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );
            let recorder = poh_recorder.recorder();
            let poh_recorder = Arc::new(Mutex::new(poh_recorder));

            let (poh_simulator, exit) = simulate_poh(record_receiver, &poh_recorder);

            poh_recorder.lock().unwrap().set_working_bank(working_bank);
            let pubkey = solana_sdk::pubkey::new_rand();
            let keypair2 = Keypair::new();
            let pubkey2 = solana_sdk::pubkey::new_rand();

            let transactions = vec![
                system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
                system_transaction::transfer(&keypair2, &pubkey2, 1, genesis_config.hash()),
            ];

            let mut results = vec![(Ok(()), None), (Ok(()), None)];
            let _ =
                BankingStage::record_transactions(bank.slot(), &transactions, &results, &recorder);
            let (_bank, (entry, _tick_height)) = entry_receiver.recv().unwrap();
            assert_eq!(entry.transactions.len(), transactions.len());

            // InstructionErrors should still be recorded
            results[0] = (
                Err(TransactionError::InstructionError(
                    1,
                    SystemError::ResultWithNegativeLamports.into(),
                )),
                None,
            );
            let (res, retryable) =
                BankingStage::record_transactions(bank.slot(), &transactions, &results, &recorder);
            res.unwrap();
            assert!(retryable.is_empty());
            let (_bank, (entry, _tick_height)) = entry_receiver.recv().unwrap();
            assert_eq!(entry.transactions.len(), transactions.len());

            // Other TransactionErrors should not be recorded
            results[0] = (Err(TransactionError::AccountNotFound), None);
            let (res, retryable) =
                BankingStage::record_transactions(bank.slot(), &transactions, &results, &recorder);
            res.unwrap();
            assert!(retryable.is_empty());
            let (_bank, (entry, _tick_height)) = entry_receiver.recv().unwrap();
            assert_eq!(entry.transactions.len(), transactions.len() - 1);

            // Once bank is set to a new bank (setting bank.slot() + 1 in record_transactions),
            // record_transactions should throw MaxHeightReached and return the set of retryable
            // txs
            let (res, retryable) = BankingStage::record_transactions(
                bank.slot() + 1,
                &transactions,
                &results,
                &recorder,
            );
            assert_matches!(res, Err(PohRecorderError::MaxHeightReached));
            // The first result was an error so it's filtered out. The second result was Ok(),
            // so it should be marked as retryable
            assert_eq!(retryable, vec![1]);
            // Should receive nothing from PohRecorder b/c record failed
            assert!(entry_receiver.try_recv().is_err());

            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_bank_filter_transaction_indexes() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let pubkey = solana_sdk::pubkey::new_rand();

        let transactions = vec![
            None,
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            None,
            None,
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            None,
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            None,
            Some(system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                1,
                genesis_config.hash(),
            )),
            None,
            None,
        ];

        let filtered_transactions = vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
        ];

        assert_eq!(
            BankingStage::filter_transaction_indexes(
                transactions.clone(),
                &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
            ),
            (filtered_transactions.clone(), vec![1, 2, 3, 6, 8, 10])
        );

        assert_eq!(
            BankingStage::filter_transaction_indexes(
                transactions,
                &[1, 2, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15],
            ),
            (filtered_transactions, vec![2, 4, 5, 9, 11, 13])
        );
    }

    #[test]
    fn test_bank_prepare_filter_for_pending_transaction() {
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let pubkey = solana_sdk::pubkey::new_rand();

        let transactions = vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
        ];

        assert_eq!(
            BankingStage::prepare_filter_for_pending_transactions(&transactions, &[2, 4, 5],),
            vec![
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Ok(())
            ]
        );

        assert_eq!(
            BankingStage::prepare_filter_for_pending_transactions(&transactions, &[0, 2, 3],),
            vec![
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Ok(()),
                Ok(()),
                Err(TransactionError::BlockhashNotFound),
                Err(TransactionError::BlockhashNotFound),
            ]
        );
    }

    #[test]
    fn test_bank_filter_valid_transaction_indexes() {
        assert_eq!(
            BankingStage::filter_valid_transaction_indexes(
                &[
                    (Err(TransactionError::BlockhashNotFound), None),
                    (Err(TransactionError::BlockhashNotFound), None),
                    (Ok(()), None),
                    (Err(TransactionError::BlockhashNotFound), None),
                    (Ok(()), None),
                    (Ok(()), None),
                ],
                &[2, 4, 5, 9, 11, 13]
            ),
            [5, 11, 13]
        );

        assert_eq!(
            BankingStage::filter_valid_transaction_indexes(
                &[
                    (Ok(()), None),
                    (Err(TransactionError::BlockhashNotFound), None),
                    (Err(TransactionError::BlockhashNotFound), None),
                    (Ok(()), None),
                    (Ok(()), None),
                    (Ok(()), None),
                ],
                &[1, 6, 7, 9, 31, 43]
            ),
            [1, 9, 31, 43]
        );
    }

    #[test]
    fn test_should_process_or_forward_packets() {
        let my_pubkey = solana_sdk::pubkey::new_rand();
        let my_pubkey1 = solana_sdk::pubkey::new_rand();
        let bank = Arc::new(Bank::default());
        assert_matches!(
            BankingStage::consume_or_forward_packets(&my_pubkey, None, Some(&bank), false, false),
            BufferedPacketsDecision::Hold
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(&my_pubkey, None, None, false, false),
            BufferedPacketsDecision::Hold
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(&my_pubkey1, None, None, false, false),
            BufferedPacketsDecision::Hold
        );

        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey,
                Some(my_pubkey1),
                None,
                false,
                false
            ),
            BufferedPacketsDecision::Forward
        );

        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey,
                Some(my_pubkey1),
                None,
                true,
                true
            ),
            BufferedPacketsDecision::Hold
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey,
                Some(my_pubkey1),
                None,
                true,
                false
            ),
            BufferedPacketsDecision::ForwardAndHold
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey,
                Some(my_pubkey1),
                Some(&bank),
                false,
                false
            ),
            BufferedPacketsDecision::Consume(_)
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey1,
                Some(my_pubkey1),
                None,
                false,
                false
            ),
            BufferedPacketsDecision::Hold
        );
        assert_matches!(
            BankingStage::consume_or_forward_packets(
                &my_pubkey1,
                Some(my_pubkey1),
                Some(&bank),
                false,
                false
            ),
            BufferedPacketsDecision::Consume(_)
        );
    }

    fn create_slow_genesis_config(lamports: u64) -> GenesisConfigInfo {
        let mut config_info = create_genesis_config(lamports);
        // For these tests there's only 1 slot, don't want to run out of ticks
        config_info.genesis_config.ticks_per_slot *= 8;
        config_info
    }

    #[test]
    fn test_bank_process_and_record_transactions() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));
        let pubkey = solana_sdk::pubkey::new_rand();

        let transactions = vec![system_transaction::transfer(
            &mint_keypair,
            &pubkey,
            1,
            genesis_config.hash(),
        )];

        let start = Arc::new(Instant::now());
        let working_bank = WorkingBank {
            bank: bank.clone(),
            start,
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.tick_height() + 1,
        };
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&ledger_path)
                .expect("Expected to be able to open database ledger");
            let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
                bank.tick_height(),
                bank.last_blockhash(),
                bank.slot(),
                Some((4, 4)),
                bank.ticks_per_slot(),
                &pubkey,
                &Arc::new(blockstore),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );
            let recorder = poh_recorder.recorder();
            let poh_recorder = Arc::new(Mutex::new(poh_recorder));

            let (poh_simulator, exit) = simulate_poh(record_receiver, &poh_recorder);

            poh_recorder.lock().unwrap().set_working_bank(working_bank);
            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            BankingStage::process_and_record_transactions(
                &bank,
                &transactions,
                &recorder,
                0,
                None,
                &gossip_vote_sender,
            )
            .0
            .unwrap();
            poh_recorder.lock().unwrap().tick();

            let mut done = false;
            // read entries until I find mine, might be ticks...
            while let Ok((_bank, (entry, _tick_height))) = entry_receiver.recv() {
                if !entry.is_tick() {
                    trace!("got entry");
                    assert_eq!(entry.transactions.len(), transactions.len());
                    assert_eq!(bank.get_balance(&pubkey), 1);
                    done = true;
                }
                if done {
                    break;
                }
            }
            trace!("done ticking");

            assert_eq!(done, true);

            let transactions = vec![system_transaction::transfer(
                &mint_keypair,
                &pubkey,
                2,
                genesis_config.hash(),
            )];

            assert_matches!(
                BankingStage::process_and_record_transactions(
                    &bank,
                    &transactions,
                    &recorder,
                    0,
                    None,
                    &gossip_vote_sender,
                )
                .0,
                Err(PohRecorderError::MaxHeightReached)
            );

            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();

            assert_eq!(bank.get_balance(&pubkey), 1);
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    fn simulate_poh(
        record_receiver: Receiver<Record>,
        poh_recorder: &Arc<Mutex<PohRecorder>>,
    ) -> (JoinHandle<()>, Arc<AtomicBool>) {
        let exit = Arc::new(AtomicBool::new(false));
        let exit_ = exit.clone();
        let poh_recorder = poh_recorder.clone();
        let tick_producer = Builder::new()
            .name("solana-simulate_poh".to_string())
            .spawn(move || loop {
                PohService::read_record_receiver_and_process(
                    &poh_recorder,
                    &record_receiver,
                    Duration::from_millis(10),
                );
                if exit_.load(Ordering::Relaxed) {
                    break;
                }
            });
        (tick_producer.unwrap(), exit)
    }

    #[test]
    fn test_bank_process_and_record_transactions_account_in_use() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));
        let pubkey = solana_sdk::pubkey::new_rand();
        let pubkey1 = solana_sdk::pubkey::new_rand();

        let transactions = vec![
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey1, 1, genesis_config.hash()),
        ];

        let start = Arc::new(Instant::now());
        let working_bank = WorkingBank {
            bank: bank.clone(),
            start,
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.tick_height() + 1,
        };
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&ledger_path)
                .expect("Expected to be able to open database ledger");
            let (poh_recorder, _entry_receiver, record_receiver) = PohRecorder::new(
                bank.tick_height(),
                bank.last_blockhash(),
                bank.slot(),
                Some((4, 4)),
                bank.ticks_per_slot(),
                &pubkey,
                &Arc::new(blockstore),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );
            let recorder = poh_recorder.recorder();
            let poh_recorder = Arc::new(Mutex::new(poh_recorder));

            poh_recorder.lock().unwrap().set_working_bank(working_bank);

            let (poh_simulator, exit) = simulate_poh(record_receiver, &poh_recorder);

            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let (result, unprocessed) = BankingStage::process_and_record_transactions(
                &bank,
                &transactions,
                &recorder,
                0,
                None,
                &gossip_vote_sender,
            );

            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();

            assert!(result.is_ok());
            assert_eq!(unprocessed.len(), 1);
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_filter_valid_packets() {
        solana_logger::setup();

        let mut all_packets = (0..16)
            .map(|packets_id| {
                let packets = Packets::new(
                    (0..32)
                        .map(|packet_id| {
                            let mut p = Packet::default();
                            p.meta.port = packets_id << 8 | packet_id;
                            p
                        })
                        .collect_vec(),
                );
                let valid_indexes = (0..32)
                    .filter_map(|x| if x % 2 != 0 { Some(x as usize) } else { None })
                    .collect_vec();
                (packets, valid_indexes, false)
            })
            .collect_vec();

        let result = BankingStage::filter_valid_packets_for_forwarding(all_packets.iter());

        assert_eq!(result.len(), 256);

        let _ = result
            .into_iter()
            .enumerate()
            .map(|(index, p)| {
                let packets_id = index / 16;
                let packet_id = (index % 16) * 2 + 1;
                assert_eq!(p.meta.port, (packets_id << 8 | packet_id) as u16);
            })
            .collect_vec();

        all_packets[0].2 = true;
        let result = BankingStage::filter_valid_packets_for_forwarding(all_packets.iter());
        assert_eq!(result.len(), 240);
    }

    #[test]
    fn test_process_transactions_returns_unprocessed_txs() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let bank = Arc::new(Bank::new_no_wallclock_throttle(&genesis_config));

        let pubkey = solana_sdk::pubkey::new_rand();

        let transactions =
            vec![
                system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash(),);
                3
            ];

        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&ledger_path)
                .expect("Expected to be able to open database ledger");
            let (poh_recorder, _entry_receiver, record_receiver) = PohRecorder::new(
                bank.tick_height(),
                bank.last_blockhash(),
                bank.slot(),
                Some((4, 4)),
                bank.ticks_per_slot(),
                &solana_sdk::pubkey::new_rand(),
                &Arc::new(blockstore),
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );

            // Poh Recorder has no working bank, so should throw MaxHeightReached error on
            // record
            let recorder = poh_recorder.recorder();

            let (poh_simulator, exit) =
                simulate_poh(record_receiver, &Arc::new(Mutex::new(poh_recorder)));

            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let (processed_transactions_count, mut retryable_txs) =
                BankingStage::process_transactions(
                    &bank,
                    &Instant::now(),
                    &transactions,
                    &recorder,
                    None,
                    &gossip_vote_sender,
                );

            assert_eq!(processed_transactions_count, 0,);

            retryable_txs.sort_unstable();
            let expected: Vec<usize> = (0..transactions.len()).collect();
            assert_eq!(retryable_txs, expected);

            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }

        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_write_persist_transaction_status() {
        solana_logger::setup();
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_slow_genesis_config(10_000);
        let bank = Arc::new(Bank::new(&genesis_config));
        let pubkey = solana_sdk::pubkey::new_rand();
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let keypair1 = Keypair::new();

        let success_tx =
            system_transaction::transfer(&mint_keypair, &pubkey, 1, genesis_config.hash());
        let success_signature = success_tx.signatures[0];
        let entry_1 = next_entry(&genesis_config.hash(), 1, vec![success_tx.clone()]);
        let ix_error_tx =
            system_transaction::transfer(&keypair1, &pubkey1, 10, genesis_config.hash());
        let ix_error_signature = ix_error_tx.signatures[0];
        let entry_2 = next_entry(&entry_1.hash, 1, vec![ix_error_tx.clone()]);
        let fail_tx =
            system_transaction::transfer(&mint_keypair, &pubkey1, 1, genesis_config.hash());
        let entry_3 = next_entry(&entry_2.hash, 1, vec![fail_tx.clone()]);
        let entries = vec![entry_1, entry_2, entry_3];

        let transactions = vec![success_tx, ix_error_tx, fail_tx];
        bank.transfer(4, &mint_keypair, &keypair1.pubkey()).unwrap();

        let start = Arc::new(Instant::now());
        let working_bank = WorkingBank {
            bank: bank.clone(),
            start,
            min_tick_height: bank.tick_height(),
            max_tick_height: bank.tick_height() + 1,
        };
        let ledger_path = get_tmp_ledger_path!();
        {
            let blockstore = Blockstore::open(&ledger_path)
                .expect("Expected to be able to open database ledger");
            let blockstore = Arc::new(blockstore);
            let (poh_recorder, _entry_receiver, record_receiver) = PohRecorder::new(
                bank.tick_height(),
                bank.last_blockhash(),
                bank.slot(),
                Some((4, 4)),
                bank.ticks_per_slot(),
                &pubkey,
                &blockstore,
                &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
                &Arc::new(PohConfig::default()),
            );
            let recorder = poh_recorder.recorder();
            let poh_recorder = Arc::new(Mutex::new(poh_recorder));

            let (poh_simulator, exit) = simulate_poh(record_receiver, &poh_recorder);

            poh_recorder.lock().unwrap().set_working_bank(working_bank);

            let shreds = entries_to_test_shreds(entries, bank.slot(), 0, true, 0);
            blockstore.insert_shreds(shreds, None, false).unwrap();
            blockstore.set_roots(&[bank.slot()]).unwrap();

            let (transaction_status_sender, transaction_status_receiver) = unbounded();
            let transaction_status_service = TransactionStatusService::new(
                transaction_status_receiver,
                Arc::new(AtomicU64::default()),
                blockstore.clone(),
                &Arc::new(AtomicBool::new(false)),
            );

            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            let _ = BankingStage::process_and_record_transactions(
                &bank,
                &transactions,
                &recorder,
                0,
                Some(TransactionStatusSender {
                    sender: transaction_status_sender,
                    enable_cpi_and_log_storage: false,
                }),
                &gossip_vote_sender,
            );

            transaction_status_service.join().unwrap();

            let confirmed_block = blockstore.get_rooted_block(bank.slot(), false).unwrap();
            assert_eq!(confirmed_block.transactions.len(), 3);

            for TransactionWithStatusMeta { transaction, meta } in
                confirmed_block.transactions.into_iter()
            {
                if transaction.signatures[0] == success_signature {
                    let meta = meta.unwrap();
                    assert_eq!(meta.status, Ok(()));
                } else if transaction.signatures[0] == ix_error_signature {
                    let meta = meta.unwrap();
                    assert_eq!(
                        meta.status,
                        Err(TransactionError::InstructionError(
                            0,
                            InstructionError::Custom(1)
                        ))
                    );
                } else {
                    assert_eq!(meta, None);
                }
            }

            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[allow(clippy::type_complexity)]
    fn setup_conflicting_transactions(
        ledger_path: &Path,
    ) -> (
        Vec<Transaction>,
        Arc<Bank>,
        Arc<Mutex<PohRecorder>>,
        Receiver<WorkingBankEntry>,
        JoinHandle<()>,
        Arc<AtomicBool>,
    ) {
        Blockstore::destroy(&ledger_path).unwrap();
        let genesis_config_info = create_slow_genesis_config(10_000);
        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = &genesis_config_info;
        let blockstore =
            Blockstore::open(&ledger_path).expect("Expected to be able to open database ledger");
        let bank = Arc::new(Bank::new(&genesis_config));
        let (poh_recorder, entry_receiver, record_receiver) = PohRecorder::new(
            bank.tick_height(),
            bank.last_blockhash(),
            bank.slot(),
            Some((4, 4)),
            bank.ticks_per_slot(),
            &solana_sdk::pubkey::new_rand(),
            &Arc::new(blockstore),
            &Arc::new(LeaderScheduleCache::new_from_bank(&bank)),
            &Arc::new(PohConfig::default()),
        );
        let poh_recorder = Arc::new(Mutex::new(poh_recorder));

        // Set up unparallelizable conflicting transactions
        let pubkey0 = solana_sdk::pubkey::new_rand();
        let pubkey1 = solana_sdk::pubkey::new_rand();
        let pubkey2 = solana_sdk::pubkey::new_rand();
        let transactions = vec![
            system_transaction::transfer(&mint_keypair, &pubkey0, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey1, 1, genesis_config.hash()),
            system_transaction::transfer(&mint_keypair, &pubkey2, 1, genesis_config.hash()),
        ];
        let (poh_simulator, exit) = simulate_poh(record_receiver, &poh_recorder);

        (
            transactions,
            bank,
            poh_recorder,
            entry_receiver,
            poh_simulator,
            exit,
        )
    }

    #[test]
    fn test_consume_buffered_packets() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let (transactions, bank, poh_recorder, _entry_receiver, poh_simulator, exit) =
                setup_conflicting_transactions(&ledger_path);
            let recorder = poh_recorder.lock().unwrap().recorder();
            let num_conflicting_transactions = transactions.len();
            let mut packets_vec = to_packets_chunked(&transactions, num_conflicting_transactions);
            assert_eq!(packets_vec.len(), 1);
            assert_eq!(packets_vec[0].packets.len(), num_conflicting_transactions);
            let all_packets = packets_vec.pop().unwrap();
            let mut buffered_packets: UnprocessedPackets = vec![(
                all_packets,
                (0..num_conflicting_transactions).into_iter().collect(),
                false,
            )]
            .into_iter()
            .collect();

            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();

            // When the working bank in poh_recorder is None, no packets should be processed
            assert!(!poh_recorder.lock().unwrap().has_bank());
            let max_tx_processing_ns = std::u128::MAX;
            BankingStage::consume_buffered_packets(
                &Pubkey::default(),
                max_tx_processing_ns,
                &poh_recorder,
                &mut buffered_packets,
                None,
                &gossip_vote_sender,
                None::<Box<dyn Fn()>>,
                None,
                &recorder,
            );
            assert_eq!(buffered_packets[0].1.len(), num_conflicting_transactions);
            // When the poh recorder has a bank, should process all non conflicting buffered packets.
            // Processes one packet per iteration of the loop
            for num_expected_unprocessed in (0..num_conflicting_transactions).rev() {
                poh_recorder.lock().unwrap().set_bank(&bank);
                BankingStage::consume_buffered_packets(
                    &Pubkey::default(),
                    max_tx_processing_ns,
                    &poh_recorder,
                    &mut buffered_packets,
                    None,
                    &gossip_vote_sender,
                    None::<Box<dyn Fn()>>,
                    None,
                    &recorder,
                );
                if num_expected_unprocessed == 0 {
                    assert!(buffered_packets.is_empty())
                } else {
                    assert_eq!(buffered_packets[0].1.len(), num_expected_unprocessed);
                }
            }
            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_consume_buffered_packets_interrupted() {
        let ledger_path = get_tmp_ledger_path!();
        {
            let (transactions, bank, poh_recorder, _entry_receiver, poh_simulator, exit) =
                setup_conflicting_transactions(&ledger_path);
            let num_conflicting_transactions = transactions.len();
            let packets_vec = to_packets_chunked(&transactions, 1);
            assert_eq!(packets_vec.len(), num_conflicting_transactions);
            for single_packets in &packets_vec {
                assert_eq!(single_packets.packets.len(), 1);
            }
            let mut buffered_packets: UnprocessedPackets = packets_vec
                .clone()
                .into_iter()
                .map(|single_packets| (single_packets, vec![0], false))
                .collect();

            let (continue_sender, continue_receiver) = unbounded();
            let (finished_packet_sender, finished_packet_receiver) = unbounded();

            let test_fn = Some(move || {
                finished_packet_sender.send(()).unwrap();
                continue_receiver.recv().unwrap();
            });
            // When the poh recorder has a bank, it should process all non conflicting buffered packets.
            // Because each conflicting transaction is in it's own `Packet` within `packets_vec`, then
            // each iteration of this loop will process one element of `packets_vec`per iteration of the
            // loop.
            let interrupted_iteration = 1;
            poh_recorder.lock().unwrap().set_bank(&bank);
            let poh_recorder_ = poh_recorder.clone();
            let recorder = poh_recorder_.lock().unwrap().recorder();
            let (gossip_vote_sender, _gossip_vote_receiver) = unbounded();
            // Start up thread to process the banks
            let t_consume = Builder::new()
                .name("consume-buffered-packets".to_string())
                .spawn(move || {
                    BankingStage::consume_buffered_packets(
                        &Pubkey::default(),
                        std::u128::MAX,
                        &poh_recorder_,
                        &mut buffered_packets,
                        None,
                        &gossip_vote_sender,
                        test_fn,
                        None,
                        &recorder,
                    );

                    // Check everything is correct. All indexes after `interrupted_iteration`
                    // should still be unprocessed
                    assert_eq!(
                        buffered_packets.len(),
                        packets_vec[interrupted_iteration + 1..].iter().count()
                    );
                    for ((remaining_unprocessed_packet, _, _forwarded), original_packet) in
                        buffered_packets
                            .iter()
                            .zip(&packets_vec[interrupted_iteration + 1..])
                    {
                        assert_eq!(
                            remaining_unprocessed_packet.packets[0],
                            original_packet.packets[0]
                        );
                    }
                })
                .unwrap();

            for i in 0..=interrupted_iteration {
                finished_packet_receiver.recv().unwrap();
                if i == interrupted_iteration {
                    poh_recorder
                        .lock()
                        .unwrap()
                        .schedule_dummy_max_height_reached_failure();
                }
                continue_sender.send(()).unwrap();
            }

            t_consume.join().unwrap();
            exit.store(true, Ordering::Relaxed);
            let _ = poh_simulator.join();
        }
        Blockstore::destroy(&ledger_path).unwrap();
    }

    #[test]
    fn test_push_unprocessed_batch_limit() {
        solana_logger::setup();
        // Create `Packets` with 1 unprocessed element
        let single_element_packets = Packets::new(vec![Packet::default()]);
        let mut unprocessed_packets: UnprocessedPackets =
            vec![(single_element_packets.clone(), vec![0], false)]
                .into_iter()
                .collect();
        // Set the limit to 2
        let batch_limit = 2;
        // Create some new unprocessed packets
        let new_packets = single_element_packets;
        let packet_indexes = vec![];

        let duplicates = Arc::new(Mutex::new((
            LruCache::new(DEFAULT_LRU_SIZE),
            PacketHasher::default(),
        )));
        let mut dropped_batches_count = 0;
        let mut newly_buffered_packets_count = 0;
        // Because the set of unprocessed `packet_indexes` is empty, the
        // packets are not added to the unprocessed queue
        BankingStage::push_unprocessed(
            &mut unprocessed_packets,
            new_packets.clone(),
            packet_indexes,
            &mut dropped_batches_count,
            &mut newly_buffered_packets_count,
            batch_limit,
            &duplicates,
        );
        assert_eq!(unprocessed_packets.len(), 1);
        assert_eq!(dropped_batches_count, 0);
        assert_eq!(newly_buffered_packets_count, 0);

        // Because the set of unprocessed `packet_indexes` is non-empty, the
        // packets are added to the unprocessed queue
        let packet_indexes = vec![0];
        BankingStage::push_unprocessed(
            &mut unprocessed_packets,
            new_packets,
            packet_indexes.clone(),
            &mut dropped_batches_count,
            &mut newly_buffered_packets_count,
            batch_limit,
            &duplicates,
        );
        assert_eq!(unprocessed_packets.len(), 2);
        assert_eq!(dropped_batches_count, 0);
        assert_eq!(newly_buffered_packets_count, 1);

        // Because we've reached the batch limit, old unprocessed packets are
        // dropped and the new one is appended to the end
        let new_packets = Packets::new(vec![Packet::from_data(
            &SocketAddr::from(([127, 0, 0, 1], 8001)),
            42,
        )
        .unwrap()]);
        assert_eq!(unprocessed_packets.len(), batch_limit);
        BankingStage::push_unprocessed(
            &mut unprocessed_packets,
            new_packets.clone(),
            packet_indexes.clone(),
            &mut dropped_batches_count,
            &mut newly_buffered_packets_count,
            batch_limit,
            &duplicates,
        );
        assert_eq!(unprocessed_packets.len(), 2);
        assert_eq!(unprocessed_packets[1].0.packets[0], new_packets.packets[0]);
        assert_eq!(dropped_batches_count, 1);
        assert_eq!(newly_buffered_packets_count, 2);

        // Check duplicates are dropped
        BankingStage::push_unprocessed(
            &mut unprocessed_packets,
            new_packets.clone(),
            packet_indexes,
            &mut dropped_batches_count,
            &mut newly_buffered_packets_count,
            3,
            &duplicates,
        );
        assert_eq!(unprocessed_packets.len(), 2);
        assert_eq!(unprocessed_packets[1].0.packets[0], new_packets.packets[0]);
        assert_eq!(dropped_batches_count, 1);
        assert_eq!(newly_buffered_packets_count, 2);
    }
}
