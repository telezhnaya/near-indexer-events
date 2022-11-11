// TODO cleanup imports in all the files in the end
use cached::SizedCache;
use clap::Parser;
use dotenv::dotenv;
use futures::StreamExt;
use std::env;
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;
use tracing_utils::DefaultSubcriberGuard;

use crate::configs::Opts;
use near_lake_framework::near_indexer_primitives;
mod configs;
mod db_adapters;
mod models;
mod rpc_helpers;
mod tracing_utils;

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

    let pool = sqlx::PgPool::connect(&env::var("DATABASE_URL")?).await?;

    let config = near_lake_framework::LakeConfigBuilder::default()
        .s3_bucket_name(opts.s3_bucket_name)
        .s3_region_name(opts.s3_region_name)
        .start_block_height(opts.start_block_height)
        .blocks_preload_pool_size(100)
        .build()?;

    let _writer_guard = init_tracing();

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
    while let Some(handle_message) = handlers.next().await {
        match handle_message {
            Ok(_block_height) => {
                // let elapsed = time_now.elapsed();
                // println!(
                //     "Elapsed time spent on block {}: {:.3?}",
                //     block_height, elapsed
                // );
                // time_now = std::time::Instant::now();
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e));
            }
        }
    }

    // propagate errors from the Lake Framework
    match lake_handle.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::Error::from(e)), // JoinError
    }
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
