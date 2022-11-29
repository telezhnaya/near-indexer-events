// TODO cleanup imports in all the files in the end
use crate::configs::Opts;
use cached::SizedCache;
use chrono::Utc;
use clap::Parser;
use dotenv::dotenv;
use futures::StreamExt;
use metrics_server::{
    init_metrics_server, BLOCK_PROCESSED_TOTAL, LAST_SEEN_BLOCK_HEIGHT, LATEST_BLOCK_TIMESTAMP_DIFF,
};
use near_lake_framework::near_indexer_primitives;
use near_primitives::utils::from_timestamp;
use std::env;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_utils::DefaultSubcriberGuard;
mod configs;
mod db_adapters;
mod metrics_server;
mod models;
mod rpc_helpers;
mod tracing_utils;
#[macro_use]
extern crate lazy_static;

pub(crate) const LOGGING_PREFIX: &str = "indexer_events";

const INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_DELAY_TIME: std::time::Duration = std::time::Duration::from_secs(120);

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct AccountWithContract {
    pub account_id: near_primitives::types::AccountId,
    pub contract_account_id: near_primitives::types::AccountId,
}

pub(crate) type FtBalanceCache =
    std::sync::Arc<Mutex<SizedCache<AccountWithContract, near_primitives::types::Balance>>>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv().ok();
    let opts: Opts = Opts::parse();

    let s3_config = aws_sdk_s3::config::Builder::from(&opts.lake_aws_sdk_config()).build();
    let config_builder = near_lake_framework::LakeConfigBuilder::default().s3_config(s3_config);

    let config = match opts.chain_id.as_str() {
        "mainnet" => config_builder.mainnet(),
        "testnet" => config_builder.testnet(),
        _ => panic!(),
    }
    .start_block_height(opts.start_block_height)
    .build()?;

    let pool = sqlx::PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let _writer_guard = init_tracing();

    tracing::info!(target: LOGGING_PREFIX, "Chain_id: {}", opts.chain_id);

    let (lake_handle, stream) = near_lake_framework::streamer(config);
    let json_rpc_client = near_jsonrpc_client::JsonRpcClient::connect(&opts.near_archival_rpc_url);

    // We want to prevent unnecessary RPC queries to find previous balance
    // It's also required because we can't query balance in the middle of the block
    let ft_balance_cache: FtBalanceCache =
        std::sync::Arc::new(Mutex::new(SizedCache::with_size(100_000)));
    // We decided to ignore invalid contracts so we need to keep the cache for it
    let contracts =
        db_adapters::contracts::ContractsHelper::restore_from_db(&pool, opts.start_block_height)
            .await?;

    tokio::spawn(async move {
        let mut handlers = tokio_stream::wrappers::ReceiverStream::new(stream)
            .map(|streamer_message| {
                handle_streamer_message(
                    streamer_message,
                    &pool,
                    &json_rpc_client,
                    &ft_balance_cache,
                    &contracts,
                )
            })
            .buffer_unordered(1usize);

        // let mut time_now = std::time::Instant::now();
        while let Some(_handle_message) = handlers.next().await {}
        //     match handle_message {
        //         Ok(_block_height) => {
        //             // let elapsed = time_now.elapsed();
        //             // println!(
        //             //     "Elapsed time spent on block {}: {:.3?}",
        //             //     block_height, elapsed
        //             // );
        //             // time_now = std::time::Instant::now();
        //         }
        //         Err(e) => {
        //             return Err(anyhow::anyhow!(e));
        //         }
        //     }
        // }
    });
        init_metrics_server().await?;
        Ok(())
    // // propagate errors from the Lake Framework
    // match lake_handle.await {
    //     Ok(Ok(())) => Ok(()),
    //     Ok(Err(e)) => Err(e),
    //     Err(e) => Err(anyhow::Error::from(e)), // JoinError
    // }
}

async fn handle_streamer_message(
    streamer_message: near_indexer_primitives::StreamerMessage,
    pool: &sqlx::Pool<sqlx::Postgres>,
    json_rpc_client: &near_jsonrpc_client::JsonRpcClient,
    ft_balance_cache: &FtBalanceCache,
    contracts: &db_adapters::contracts::ContractsHelper,
) -> anyhow::Result<u64> {
    if streamer_message.block.header.height % 100 == 0 {
        tracing::info!(
            target: crate::LOGGING_PREFIX,
            "{} / shards {}",
            streamer_message.block.header.height,
            streamer_message.shards.len()
        );
    }

    db_adapters::events::store_events(
        pool,
        json_rpc_client,
        &streamer_message,
        ft_balance_cache,
        contracts,
    )
    .await?;

    let now = Utc::now();
    let block_timestamp = from_timestamp(streamer_message.block.header.timestamp_nanosec);

    LATEST_BLOCK_TIMESTAMP_DIFF.set((now - block_timestamp).num_seconds() as f64);
    LAST_SEEN_BLOCK_HEIGHT.set(streamer_message.block.header.height.try_into().unwrap());
    BLOCK_PROCESSED_TOTAL.inc();

    Ok(streamer_message.block.header.height)
}

fn init_tracing() -> DefaultSubcriberGuard {
    let mut env_filter = EnvFilter::new("near_lake_framework=info,indexer_events=info");

    if let Ok(rust_log) = env::var("RUST_LOG") {
        if !rust_log.is_empty() {
            for directive in rust_log.split(',').filter_map(|s| match s.parse() {
                Ok(directive) => Some(directive),
                Err(err) => {
                    tracing::warn!(
                        target: crate::LOGGING_PREFIX,
                        "Ignoring directive `{}`: {}",
                        s,
                        err
                    );
                    None
                }
            }) {
                env_filter = env_filter.add_directive(directive);
            }
        }
    }

    let (non_blocking_writer, _guard) = tracing_appender::non_blocking(std::io::stderr());

    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(env_filter)
        .with_writer(non_blocking_writer)
        .finish();

    DefaultSubcriberGuard {
        subscriber_guard: tracing::subscriber::set_default(subscriber),
        writer_guard: _guard,
    }
}