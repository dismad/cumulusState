use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use zebra_chain::{
    block::Height,
    parameters::Network,
    serialization::ZcashSerialize,
    transaction::Transaction,
};
use zebra_state::{
    constants::state_database_format_version_in_code,
    Config, TransactionLocation, ZebraDb,
};

#[derive(Parser, Debug)]
#[command(
    name = "cumulusState",
    about = "Read-only multipurpose scanner for a Zebra v7 / v29 state database",
    long_about = "Opens a Zebra state database read-only and walks transactions by height once.\n\
                  Does not write any files. Stop zebrad for the most reliable results.\n\
                  Requires the indexer feature on the state for reverse spend lookups."
)]
struct Args {
    /// Path to the Zebra cache directory (parent of state/v29/...)
    #[arg(long, default_value = "~/.cache/zebra")]
    cache_dir: String,
    /// Network
    #[arg(long, default_value = "mainnet", value_parser = ["mainnet", "testnet"])]
    network: String,
    /// Start height (inclusive)
    #[arg(long, default_value_t = 0)]
    start: u32,
    /// End height (inclusive). Defaults to tip if omitted.
    #[arg(long)]
    end: Option<u32>,
    /// Scan the last N blocks from tip (overrides start/end)
    #[arg(long)]
    blocks: Option<u32>,
    /// Emit JSON instead of plain text
    #[arg(long)]
    json: bool,
    /// Also emit per-day aggregates (in addition to totals)
    #[arg(long)]
    per_day: bool,
    /// Number of parallel chunks (default: number of CPUs)
    #[arg(long)]
    threads: Option<usize>,
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    Expiry,
    Counts,
    Shielded,
    Types,
    Addresses,
    Blocks,
    Locktime,
    Spends,
    All,
}

#[derive(Default, Serialize, Clone)]
struct ExpiryStats {
    total: u64,
    zero: u64,
    nonzero: u64,
    pre_overwinter: u64,
    zero_fraction: f64,
    delta_histogram: HashMap<i64, u64>,
}
#[derive(Default, Serialize, Clone)]
struct CountStats { blocks: u64, txs: u64 }
#[derive(Default, Serialize, Clone)]
struct ShieldedStats {
    sapling_spends: u64,
    sapling_outputs: u64,
    orchard_actions: u64,
    ironwood_actions: u64,
    sprout_joinsplits: u64,
    sprout_vpub_old: u64,
    sprout_vpub_new: u64,
    sapling_value_balance: i64,
    orchard_value_balance: i64,
    ironwood_value_balance: i64,
}
#[derive(Default, Serialize, Clone)]
struct AddressStats { unique_receivers: u64, total_receiving_outputs: u64 }
#[derive(Default, Serialize, Clone)]
struct TypeStats {
    v1: u64, v2: u64, v3: u64, v4: u64, v5: u64, v6: u64, other: u64, coinbase: u64, user: u64,
    fully_shielded: u64, mixed: u64, transparent_only: u64,
}
#[derive(Default, Serialize, Clone)]
struct TreeStats { sapling: u64, orchard: u64, ironwood: u64, sprout: u64 }
#[derive(Default, Serialize, Clone)]
struct BlockStats { blocks: u64, total_txs: u64, avg_txs_per_block: f64, max_txs_per_block: u64, total_tx_bytes: u64 }
#[derive(Default, Serialize, Clone)]
struct LocktimeStats { nonzero: u64, zero: u64, high_sequence: u64 }
#[derive(Default, Serialize, Clone)]
struct SpendStats {
    indexer_populated: bool, probed: u64, outpoint_hits: u64,
    sapling_nullifier_hits: u64, orchard_nullifier_hits: u64, ironwood_nullifier_hits: u64,
    sprout_nullifier_hits: u64,
}
#[derive(Default, Serialize, Clone)]
struct DayStats {
    day: u32,
    start: u32,
    end: u32,
    expiry: ExpiryStats,
    counts: CountStats,
    shielded: ShieldedStats,
    types: TypeStats,
    addresses: AddressStats,
    anchors: AnchorStats,
    trees: TreeStats,
    blocks: BlockStats,
    locktime: LocktimeStats,
    spends: SpendStats,
}

#[derive(Serialize)]
struct AllOutput {
    expiry: ExpiryStats,
    counts: CountStats,
    shielded: ShieldedStats,
    types: TypeStats,
    addresses: AddressStats,
    anchors: AnchorStats,
    trees: TreeStats,
    blocks: BlockStats,
    locktime: LocktimeStats,
    spends: SpendStats,
    #[serde(skip_serializing_if = "Option::is_none")]
    per_day: Option<Vec<DayStats>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum OneOutput {
    Expiry(ExpiryStats),
    Counts(CountStats),
    Shielded(ShieldedStats),
    Types(TypeStats),
    Addresses(AddressStats),
    Blocks(BlockStats),
    Locktime(LocktimeStats),
    Spends(SpendStats),
}

#[derive(Default, Clone)]
struct Acc {
    expiry: ExpiryStats,
    counts: CountStats,
    shielded: ShieldedStats,
    types: TypeStats,
    addresses: AddressStats,
    anchors: AnchorStats,
    trees: TreeStats,
    locktime: LocktimeStats,
    spends: SpendStats,
    receivers: HashSet<String>,
    per_block_txs: Vec<u64>,
    day_receivers: HashSet<String>,
    days: Vec<DayStats>,
    current_day: DayStats,
    day_tx_counts: Vec<u64>,
    total_tx_bytes: u64,
}

impl Acc {
    fn finish_day(&mut self) {
        let mut d = std::mem::take(&mut self.current_day);
        d.addresses.unique_receivers = self.day_receivers.len() as u64;
        self.day_receivers.clear();
        let total: u64 = self.day_tx_counts.iter().sum();
        let max = self.day_tx_counts.iter().copied().max().unwrap_or(0);
        let n = self.day_tx_counts.len() as u64;
        d.blocks = BlockStats {
            blocks: n,
            total_txs: total,
            avg_txs_per_block: if n > 0 { total as f64 / n as f64 } else { 0.0 },
            max_txs_per_block: max,
            total_tx_bytes: d.blocks.total_tx_bytes,
        };
        self.day_tx_counts.clear();
        if d.expiry.zero + d.expiry.nonzero > 0 {
            d.expiry.zero_fraction = d.expiry.zero as f64 / (d.expiry.zero + d.expiry.nonzero) as f64;
        }
        self.days.push(d);
    }

    fn finalize(&mut self) {
        self.addresses.unique_receivers = self.receivers.len() as u64;
        if self.expiry.zero + self.expiry.nonzero > 0 {
            self.expiry.zero_fraction = self.expiry.zero as f64 / (self.expiry.zero + self.expiry.nonzero) as f64;
        }
    }

    fn merge(&mut self, other: Acc) {
        self.expiry.total += other.expiry.total;
        self.expiry.zero += other.expiry.zero;
        self.expiry.nonzero += other.expiry.nonzero;
        self.expiry.pre_overwinter += other.expiry.pre_overwinter;
        for (k, v) in other.expiry.delta_histogram {
            *self.expiry.delta_histogram.entry(k).or_default() += v;
        }
        self.counts.txs += other.counts.txs;
        self.shielded.sapling_spends += other.shielded.sapling_spends;
        self.shielded.sapling_outputs += other.shielded.sapling_outputs;
        self.shielded.orchard_actions += other.shielded.orchard_actions;
        self.shielded.ironwood_actions += other.shielded.ironwood_actions;
        self.shielded.sprout_joinsplits += other.shielded.sprout_joinsplits;
        self.shielded.sprout_vpub_old += other.shielded.sprout_vpub_old;
        self.shielded.sprout_vpub_new += other.shielded.sprout_vpub_new;
        self.shielded.sapling_value_balance += other.shielded.sapling_value_balance;
        self.shielded.orchard_value_balance += other.shielded.orchard_value_balance;
        self.shielded.ironwood_value_balance += other.shielded.ironwood_value_balance;
        self.types.v1 += other.types.v1;
        self.types.v2 += other.types.v2;
        self.types.v3 += other.types.v3;
        self.types.v4 += other.types.v4;
        self.types.v5 += other.types.v5;
        self.types.v6 += other.types.v6;
        self.types.other += other.types.other;
        self.types.coinbase += other.types.coinbase;
        self.types.user += other.types.user;
        self.types.fully_shielded += other.types.fully_shielded;
        self.types.mixed += other.types.mixed;
        self.types.transparent_only += other.types.transparent_only;
        self.addresses.total_receiving_outputs += other.addresses.total_receiving_outputs;
        self.anchors.sapling += other.anchors.sapling;
        self.anchors.orchard += other.anchors.orchard;
        self.anchors.ironwood += other.anchors.ironwood;
        self.receivers.extend(other.receivers);
        self.locktime.nonzero += other.locktime.nonzero;
        self.locktime.zero += other.locktime.zero;
        self.locktime.high_sequence += other.locktime.high_sequence;
        self.spends.probed += other.spends.probed;
        self.spends.outpoint_hits += other.spends.outpoint_hits;
        self.spends.sapling_nullifier_hits += other.spends.sapling_nullifier_hits;
        self.spends.orchard_nullifier_hits += other.spends.orchard_nullifier_hits;
        self.spends.ironwood_nullifier_hits += other.spends.ironwood_nullifier_hits;
        self.spends.sprout_nullifier_hits += other.spends.sprout_nullifier_hits;
        self.per_block_txs.extend(other.per_block_txs);
        self.days.extend(other.days);
        self.total_tx_bytes += other.total_tx_bytes;
    }
}

fn accumulate(acc: &mut Acc, height: Height, tx: &Transaction, network: &Network, db: &ZebraDb) {
    acc.expiry.total += 1;
    acc.current_day.expiry.total += 1;

    if tx.is_coinbase() {
        acc.types.coinbase += 1;
        acc.current_day.types.coinbase += 1;
        return;
    }

    match expiry_height_of(tx) {
        None => {
            acc.expiry.pre_overwinter += 1;
            acc.current_day.expiry.pre_overwinter += 1;
        }
        Some(0) => {
            acc.expiry.zero += 1;
            acc.current_day.expiry.zero += 1;
        }
        Some(exp) => {
            acc.expiry.nonzero += 1;
            acc.current_day.expiry.nonzero += 1;
            let delta = exp as i64 - height.0 as i64;
            *acc.expiry.delta_histogram.entry(delta).or_default() += 1;
            *acc.current_day.expiry.delta_histogram.entry(delta).or_default() += 1;
        }
    }

    acc.types.user += 1;
    acc.current_day.types.user += 1;
    match tx.version() {
        1 => { acc.types.v1 += 1; acc.current_day.types.v1 += 1; }
        2 => { acc.types.v2 += 1; acc.current_day.types.v2 += 1; }
        3 => { acc.types.v3 += 1; acc.current_day.types.v3 += 1; }
        4 => { acc.types.v4 += 1; acc.current_day.types.v4 += 1; }
        5 => { acc.types.v5 += 1; acc.current_day.types.v5 += 1; }
        6 => { acc.types.v6 += 1; acc.current_day.types.v6 += 1; }
        _ => { acc.types.other += 1; acc.current_day.types.other += 1; }
    }

    let has_t_in = !tx.inputs().is_empty();
    let has_t_out = !tx.outputs().is_empty();
    let sap_spends = tx.sapling_spends().count() as u64;
    let sap_outs = tx.sapling_outputs().count() as u64;
    let orchard = tx.orchard_actions().count() as u64;
    let ironwood = tx.ironwood_actions().count() as u64;
    let sprout = tx.sprout_joinsplit_descriptions().count() as u64;
    let mut sprout_vpub_old = 0u64;
    let mut sprout_vpub_new = 0u64;
    for js in tx.sprout_joinsplit_descriptions() {
        sprout_vpub_old += i64::from(js.vpub_old()) as u64;
        sprout_vpub_new += i64::from(js.vpub_new()) as u64;
    }
    let sapling_vb = tx.sapling_value_balance().sapling_amount().zatoshis() as i64;
    let orchard_vb = tx.orchard_value_balance().orchard_amount().zatoshis() as i64;
    let ironwood_vb = tx.ironwood_value_balance().ironwood_amount().zatoshis() as i64;
    let tx_size = tx.zcash_serialized_size() as u64;
    let sapling_anchors = tx.sapling_anchors().len() as u64;
    let orchard_anchors = if tx.has_orchard_shielded_data() { 1 } else { 0 };
    let ironwood_anchors = if tx.has_ironwood_shielded_data() { 1 } else { 0 };

    let has_shielded = sap_spends > 0 || sap_outs > 0 || orchard > 0 || ironwood > 0 || sprout > 0;

    acc.shielded.sapling_spends += sap_spends;
    acc.shielded.sapling_outputs += sap_outs;
    acc.shielded.orchard_actions += orchard;
    acc.shielded.ironwood_actions += ironwood;
    acc.shielded.sprout_joinsplits += sprout;
    acc.shielded.sprout_vpub_old += sprout_vpub_old;
    acc.shielded.sprout_vpub_new += sprout_vpub_new;
    acc.shielded.sapling_value_balance += sapling_vb;
    acc.shielded.orchard_value_balance += orchard_vb;
    acc.shielded.ironwood_value_balance += ironwood_vb;
    acc.total_tx_bytes += tx_size;
    acc.current_day.blocks.total_tx_bytes += tx_size;
    acc.anchors.sapling += sapling_anchors;
    acc.anchors.orchard += orchard_anchors;
    acc.anchors.ironwood += ironwood_anchors;
    acc.current_day.anchors.sapling += sapling_anchors;
    acc.current_day.anchors.orchard += orchard_anchors;
    acc.current_day.anchors.ironwood += ironwood_anchors;
    acc.current_day.shielded.sapling_spends += sap_spends;
    acc.current_day.shielded.sapling_outputs += sap_outs;
    acc.current_day.shielded.orchard_actions += orchard;
    acc.current_day.shielded.ironwood_actions += ironwood;
    acc.current_day.shielded.sprout_joinsplits += sprout;
    acc.current_day.shielded.sprout_vpub_old += sprout_vpub_old;
    acc.current_day.shielded.sprout_vpub_new += sprout_vpub_new;
    acc.current_day.shielded.sapling_value_balance += sapling_vb;
    acc.current_day.shielded.orchard_value_balance += orchard_vb;
    acc.current_day.shielded.ironwood_value_balance += ironwood_vb;

    if has_shielded && !has_t_in && !has_t_out {
        acc.types.fully_shielded += 1;
        acc.current_day.types.fully_shielded += 1;
    } else if has_shielded && (has_t_in || has_t_out) {
        acc.types.mixed += 1;
        acc.current_day.types.mixed += 1;
    } else if has_t_in || has_t_out {
        acc.types.transparent_only += 1;
        acc.current_day.types.transparent_only += 1;
    }

    for out in tx.outputs() {
        if let Some(addr) = out.address(network) {
            let s = addr.to_string();
            acc.receivers.insert(s.clone());
            acc.day_receivers.insert(s);
            acc.addresses.total_receiving_outputs += 1;
            acc.current_day.addresses.total_receiving_outputs += 1;
        }
    }

    if tx.raw_lock_time() > 0 {
        acc.locktime.nonzero += 1;
        acc.current_day.locktime.nonzero += 1;
    } else {
        acc.locktime.zero += 1;
        acc.current_day.locktime.zero += 1;
    }
    if let Some(inp) = tx.inputs().first() {
        if inp.sequence() > 0 {
            acc.locktime.high_sequence += 1;
            acc.current_day.locktime.high_sequence += 1;
        }
    }

    for inp in tx.inputs() {
        if let Some(op) = inp.outpoint() {
            acc.spends.probed += 1;
            acc.current_day.spends.probed += 1;
            if acc.spends.indexer_populated && db.spending_tx_loc(&op).is_some() {
                acc.spends.outpoint_hits += 1;
                acc.current_day.spends.outpoint_hits += 1;
            }
        }
    }
    for nf in tx.sapling_nullifiers() {
        acc.spends.probed += 1;
        acc.current_day.spends.probed += 1;
        if acc.spends.indexer_populated && db.sapling_revealing_tx_loc(&nf).is_some() {
            acc.spends.sapling_nullifier_hits += 1;
            acc.current_day.spends.sapling_nullifier_hits += 1;
        }
    }
    for nf in tx.orchard_nullifiers() {
        acc.spends.probed += 1;
        acc.current_day.spends.probed += 1;
        if acc.spends.indexer_populated && db.orchard_revealing_tx_loc(&nf).is_some() {
            acc.spends.orchard_nullifier_hits += 1;
            acc.current_day.spends.orchard_nullifier_hits += 1;
        }
    }
    for nf in tx.ironwood_nullifiers() {
        acc.spends.probed += 1;
        acc.current_day.spends.probed += 1;
        if acc.spends.indexer_populated && db.ironwood_revealing_tx_loc(&nf).is_some() {
            acc.spends.ironwood_nullifier_hits += 1;
            acc.current_day.spends.ironwood_nullifier_hits += 1;
        }
    }
    for nf in tx.sprout_nullifiers() {
        acc.spends.probed += 1;
        acc.current_day.spends.probed += 1;
        if acc.spends.indexer_populated && db.sprout_revealing_tx_loc(&nf).is_some() {
            acc.spends.sprout_nullifier_hits += 1;
            acc.current_day.spends.sprout_nullifier_hits += 1;
        }
    }
}

fn scan_chunk(
    db: &ZebraDb,
    start: u32,
    end: u32,
    network: &Network,
    per_day: bool,
    indexer_populated: bool,
) -> Acc {
    let mut acc = Acc::default();
    acc.spends.indexer_populated = indexer_populated;
    acc.current_day.spends.indexer_populated = indexer_populated;
const BLOSSOM_HEIGHT: u32 = 653_600;
const NU7_HEIGHT: u32 = 3_543_405;

fn blocks_per_day(h: u32) -> u32 {
    if h < BLOSSOM_HEIGHT {
        576   // 150s pre-Blossom
    } else if h < NU7_HEIGHT {
        1152  // 75s Blossom to NU7
    } else {
        3456  // 25s NU7+
    }
}
    let mut day_start = start;
    acc.current_day.start = start;
    let mut day_limit = blocks_per_day(start);

    for h in start..=end {
        let height = Height(h);
        let mut block_txs = 0u64;
        let txs = db.transactions_by_location_range(
            TransactionLocation::from_index(height, 0)
                ..=TransactionLocation::max_for_height(height),
        );
        for (_loc, tx) in txs {
            block_txs += 1;
            accumulate(&mut acc, height, &tx, network, db);
        }
        if let Some(tree) = db.sapling_tree_by_height(&height) {
            acc.trees.sapling = tree.position().map(|p| u64::from(p)).unwrap_or(0);
            acc.current_day.trees.sapling = acc.trees.sapling;
        }
        if let Some(tree) = db.orchard_tree_by_height(&height) {
            acc.trees.orchard = tree.position().map(|p| u64::from(p)).unwrap_or(0);
            acc.current_day.trees.orchard = acc.trees.orchard;
        }
        if let Some(tree) = db.ironwood_tree_by_height(&height) {
            acc.trees.ironwood = tree.position().map(|p| u64::from(p)).unwrap_or(0);
            acc.current_day.trees.ironwood = acc.trees.ironwood;
        }
        acc.per_block_txs.push(block_txs);
        acc.day_tx_counts.push(block_txs);
        acc.counts.txs += block_txs;
        acc.current_day.counts.txs += block_txs;
        acc.current_day.counts.blocks += 1;
        // tx size is accumulated inside accumulate via a side channel; track here if needed

        if per_day && (h - day_start + 1 >= day_limit || h == end) {
            acc.current_day.end = h;
            acc.finish_day();
            day_start = h + 1;
            day_limit = blocks_per_day(day_start);
            acc.current_day = DayStats::default();
            acc.current_day.start = day_start;
            acc.current_day.spends.indexer_populated = indexer_populated;
        }
    }
    acc
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    let cache_dir = expand_tilde(&args.cache_dir);
    let network = match args.network.as_str() {
        "mainnet" => Network::Mainnet,
        "testnet" => Network::new_default_testnet(),
        _ => unreachable!(),
    };

    info!("Opening Zebra state read-only at {}", cache_dir.display());
    let mut config = Config::default();
    config.cache_dir = cache_dir;
    config.ephemeral = false;
    config.delete_old_database = false;

    let db = Arc::new(ZebraDb::new(
        &config, "state", &state_database_format_version_in_code(),
        &network, true, std::iter::empty::<String>(), true,
    ).context("failed to open Zebra state database read-only")?);

    let tip = db.finalized_tip_height().unwrap_or(Height(0));
    let (start, end) = if let Some(n) = args.blocks {
        let end = tip.0;
        let start = end.saturating_sub(n.saturating_sub(1));
        (start, end)
    } else {
        let end = args.end.unwrap_or(tip.0).min(tip.0);
        (args.start, end)
    };
    info!("Scanning {}..={} (tip {})", start, end, tip.0);

    let threads = args.threads.unwrap_or_else(num_cpus::get).max(1);
    let range_len = end.saturating_sub(start).saturating_add(1);
    let num_chunks = threads * 8;
    let chunk_size = (range_len / num_chunks as u32).max(1);

    let pb = ProgressBar::new(range_len as u64);
    pb.set_style(
        ProgressStyle::with_template("{bar:40} {pos}/{len} {percent}% [{elapsed}<{eta}]")
            .unwrap()
            .progress_chars("##-"),
    );

    let indexer_populated = indexer_looks_populated(&db);

    let chunks: Vec<(u32, u32)> = (0..threads)
        .map(|i| {
            let cs = start + i as u32 * chunk_size;
            let ce = if i + 1 == threads { end } else { (cs + chunk_size - 1).min(end) };
            (cs, ce)
        })
        .filter(|(s, e)| s <= e)
        .collect();

    let partials: Vec<Acc> = chunks
        .into_par_iter()
        .map(|(cs, ce)| {
            let acc = scan_chunk(&db, cs, ce, &network, args.per_day, indexer_populated);
            pb.inc((ce - cs + 1) as u64);
            acc
        })
        .collect();

    pb.finish_with_message("done");

    let mut acc = Acc::default();
    acc.spends.indexer_populated = indexer_populated;
    for p in partials {
        acc.merge(p);
    }
    // Order day windows by start height and assign sequential day index
    acc.days.sort_by_key(|d| d.start);
    for (i, d) in acc.days.iter_mut().enumerate() {
        d.day = i as u32;
    }
    acc.finalize();

    let total_txs: u64 = acc.per_block_txs.iter().sum();
    let max_txs = acc.per_block_txs.iter().copied().max().unwrap_or(0);
    let n_blocks = acc.per_block_txs.len() as u64;
    let blocks = BlockStats {
        blocks: n_blocks,
        total_txs,
        avg_txs_per_block: if n_blocks > 0 { total_txs as f64 / n_blocks as f64 } else { 0.0 },
        max_txs_per_block: max_txs,
        total_tx_bytes: acc.total_tx_bytes,
    };
    acc.counts.blocks = n_blocks;

    match args.cmd {
        Command::All => {
            let out = AllOutput {
                expiry: acc.expiry,
                counts: acc.counts,
                shielded: acc.shielded,
                types: acc.types,
                addresses: acc.addresses,
                anchors: acc.anchors,
                blocks,
                locktime: acc.locktime,
                spends: acc.spends,
                per_day: if args.per_day { Some(acc.days) } else { None },
            };
            if args.json {
                writeln!(io::stdout(), "{}", serde_json::to_string_pretty(&out)?)?;
            } else {
                print_all(&out);
            }
        }
        other => {
            let out = match other {
                Command::Expiry => OneOutput::Expiry(acc.expiry),
                Command::Counts => OneOutput::Counts(acc.counts),
                Command::Shielded => OneOutput::Shielded(acc.shielded),
                Command::Types => OneOutput::Types(acc.types),
                Command::Addresses => OneOutput::Addresses(acc.addresses),
                Command::Blocks => OneOutput::Blocks(blocks),
                Command::Locktime => OneOutput::Locktime(acc.locktime),
                Command::Spends => OneOutput::Spends(acc.spends),
                Command::All => unreachable!(),
            };
            if args.json {
                writeln!(io::stdout(), "{}", serde_json::to_string_pretty(&out)?)?;
            } else {
                print_one(&out);
            }
        }
    }
    Ok(())
}

fn print_all(out: &AllOutput) {
    print_expiry(&out.expiry);
    print_counts(&out.counts);
    print_shielded(&out.shielded);
    print_types(&out.types);
    print_addresses(&out.addresses);
    print_anchors(&out.anchors);
    print_blocks(&out.blocks);
    print_locktime(&out.locktime);
    print_spends(&out.spends);
    if let Some(days) = &out.per_day {
        println!("=== Per day ===");
        for d in days {
            println!(
                "day {}: blocks={} txs={} zero_expiry={} sapling_spends={}",
                d.day, d.counts.blocks, d.counts.txs, d.expiry.zero, d.shielded.sapling_spends
            );
        }
    }
}
fn print_one(out: &OneOutput) {
    match out {
        OneOutput::Expiry(s) => print_expiry(s),
        OneOutput::Counts(s) => print_counts(s),
        OneOutput::Shielded(s) => print_shielded(s),
        OneOutput::Types(s) => print_types(s),
        OneOutput::Addresses(s) => print_addresses(s),
        OneOutput::Blocks(s) => print_blocks(s),
        OneOutput::Locktime(s) => print_locktime(s),
        OneOutput::Spends(s) => print_spends(s),
    }
}
fn print_expiry(s: &ExpiryStats) {
    println!("=== Expiry ===");
    println!("total: {}  zero: {}  nonzero: {}  pre: {}", s.total, s.zero, s.nonzero, s.pre_overwinter);
    println!("zero fraction: {:.4}", s.zero_fraction);
    let mut deltas: Vec<_> = s.delta_histogram.iter().collect();
    deltas.sort_by_key(|(d, _)| *d);
    println!("delta histogram (expiry - height), top 20:");
    for (d, c) in deltas.iter().take(20) {
        println!("  {d:+6}: {c}");
    }
}
fn print_counts(s: &CountStats) { println!("=== Counts ===\nblocks: {}  txs: {}", s.blocks, s.txs); }
fn print_shielded(s: &ShieldedStats) {
    println!("=== Shielded actions ===");
    println!("sapling spends: {}\nsapling outputs: {}\norchard actions: {}\nironwood actions: {}\nsprout joinsplits: {}\nsprout vpub_old: {}\nsprout vpub_new: {}\nsapling value balance: {}\norchard value balance: {}\nironwood value balance: {}",
        s.sapling_spends, s.sapling_outputs, s.orchard_actions, s.ironwood_actions, s.sprout_joinsplits,
        s.sprout_vpub_old, s.sprout_vpub_new, s.sapling_value_balance, s.orchard_value_balance, s.ironwood_value_balance);
}
fn print_types(s: &TypeStats) {
    println!("=== Types ===");
    println!("v1: {}  v2: {}  v3: {}  v4: {}  v5: {}  v6: {}  other: {}", s.v1, s.v2, s.v3, s.v4, s.v5, s.v6, s.other);
    println!("coinbase: {}  user: {}", s.coinbase, s.user);
    println!("fully shielded: {}  mixed: {}  transparent-only: {}", s.fully_shielded, s.mixed, s.transparent_only);
}
fn print_anchors(s: &AnchorStats) {
    println!("=== Anchors cited ===\nsapling: {}\norchard: {}\nironwood: {}", s.sapling, s.orchard, s.ironwood);
}
fn print_addresses(s: &AddressStats) {
    println!("=== Transparent addresses ===\nunique receivers: {}\ntotal receiving outputs: {}",
        s.unique_receivers, s.total_receiving_outputs);
}
fn print_blocks(s: &BlockStats) {
    println!("=== Blocks ===\nblocks: {}  total txs: {}  avg txs/block: {:.2}  max: {}  total tx bytes: {}",
        s.blocks, s.total_txs, s.avg_txs_per_block, s.max_txs_per_block, s.total_tx_bytes);
}
fn print_locktime(s: &LocktimeStats) {
    println!("=== Locktime / sequence ===\nnonzero locktime: {}  zero: {}\ninputs with sequence > 0 (sampled): {}",
        s.nonzero, s.zero, s.high_sequence);
}
fn print_spends(s: &SpendStats) {
    println!("=== Spends (indexer) ===");
    println!("indexer_populated: {}\nprobed: {}\noutpoint hits: {}\nsapling nullifier hits: {}\norchard nullifier hits: {}\nironwood nullifier hits: {}\nsprout nullifier hits: {}",
        s.indexer_populated, s.probed, s.outpoint_hits, s.sapling_nullifier_hits, s.orchard_nullifier_hits, s.ironwood_nullifier_hits, s.sprout_nullifier_hits);
}

fn indexer_looks_populated(db: &ZebraDb) -> bool {
    let tip = db.finalized_tip_height().unwrap_or(Height(0));
    let probe_from = tip.0.saturating_sub(1000);
    for h in probe_from..=tip.0 {
        let height = Height(h);
        let txs = db.transactions_by_location_range(
            TransactionLocation::from_index(height, 0)
                ..=TransactionLocation::max_for_height(height),
        );
        for (_loc, tx) in txs {
            if tx.is_coinbase() { continue; }
            for inp in tx.inputs() {
                if let Some(op) = inp.outpoint() {
                    if db.spending_tx_loc(&op).is_some() { return true; }
                }
            }
        }
    }
    false
}

fn expiry_height_of(tx: &Transaction) -> Option<u32> { tx.expiry_height().map(|h| h.0) }
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(stripped);
        }
    }
    PathBuf::from(path)
}
