use crate::db_adapters::events::Event;
use crate::db_adapters::legacy_ft;
use crate::models;
use crate::models::coin_events::CoinEvent;
use bigdecimal::BigDecimal;
use near_lake_framework::near_indexer_primitives;
use near_primitives::types::AccountId;
use near_primitives::views::{ActionView, ExecutionStatusView, ReceiptEnumView};
use serde::Deserialize;
use std::ops::Mul;
use std::str::FromStr;

#[derive(Deserialize, Debug, Clone)]
struct FtNew {
    // pub metadata: ...,
    pub owner_id: AccountId,
    pub total_supply: String,
}

#[derive(Deserialize, Debug, Clone)]
struct FtTransfer {
    pub receiver_id: AccountId,
    pub amount: String,
    pub memo: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct FtRefund {
    pub receiver_id: AccountId,
    pub sender_id: AccountId,
    pub amount: String,
    pub memo: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct NearWithdraw {
    pub amount: String,
}

pub(crate) async fn store_tkn_near(
    pool: &sqlx::Pool<sqlx::Postgres>,
    json_rpc_client: &near_jsonrpc_client::JsonRpcClient,
    shard_id: &near_indexer_primitives::types::ShardId,
    receipt_execution_outcomes: &[near_indexer_primitives::IndexerExecutionOutcomeWithReceipt],
    block_header: &near_indexer_primitives::views::BlockHeaderView,
    ft_balance_cache: &crate::FtBalanceCache,
) -> anyhow::Result<()> {
    let mut events: Vec<CoinEvent> = vec![];

    for outcome in receipt_execution_outcomes {
        if !is_tkn_near_contract(outcome.execution_outcome.outcome.executor_id.as_str()) {
            continue;
        }
        if let ReceiptEnumView::Action { actions, .. } = &outcome.receipt.receipt {
            for action in actions {
                process_tkn_near_functions(
                    &mut events,
                    json_rpc_client,
                    shard_id,
                    block_header,
                    ft_balance_cache,
                    action,
                    outcome,
                )
                .await?;
            }
        }
    }

    models::chunked_insert(pool, &events).await?;
    Ok(())
}

fn is_tkn_near_contract(contract_id: &str) -> bool {
    if let Some(contract_prefix) = contract_id.strip_suffix(".tkn.near") {
        lazy_static::lazy_static! {
            static ref RE: regex::Regex = regex::Regex::new(r"^[a-z0-9\-]+$").unwrap();
        }
        RE.is_match(contract_prefix)
    } else {
        false
    }
}

async fn process_tkn_near_functions(
    events: &mut Vec<CoinEvent>,
    json_rpc_client: &near_jsonrpc_client::JsonRpcClient,
    shard_id: &near_indexer_primitives::types::ShardId,
    block_header: &near_indexer_primitives::views::BlockHeaderView,
    cache: &crate::FtBalanceCache,
    action: &ActionView,
    outcome: &near_indexer_primitives::IndexerExecutionOutcomeWithReceipt,
) -> anyhow::Result<()> {
    let (method_name, args, deposit) = match action {
        ActionView::FunctionCall {
            method_name,
            args,
            deposit,
            ..
        } => (method_name, args, deposit),
        _ => return Ok(()),
    };

    let decoded_args = base64::decode(args)?;

    if method_name == "storage_deposit" {
        return Ok(());
    }

    // may mint the tokens
    if method_name == "new" {
        let args = match serde_json::from_slice::<FtNew>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => return Ok(()),
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };
        let delta = BigDecimal::from_str(&args.total_supply)?;
        let base = legacy_ft::get_base(
            Event::TknNear,
            shard_id,
            events.len(),
            outcome,
            block_header,
        )?;
        let custom = legacy_ft::EventCustom {
            affected_id: args.owner_id,
            involved_id: None,
            delta,
            cause: "MINT".to_string(),
            memo: None,
        };
        events.push(
            legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom).await?,
        );
        return Ok(());
    }

    // MINT produces 1 event, where involved_account_id is NULL.
    if method_name == "near_deposit" {
        let delta = BigDecimal::from_str(&deposit.to_string())?;
        let base = legacy_ft::get_base(
            Event::TknNear,
            shard_id,
            events.len(),
            outcome,
            block_header,
        )?;
        let custom = legacy_ft::EventCustom {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: None,
            delta,
            cause: "MINT".to_string(),
            memo: None,
        };
        events.push(
            legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom).await?,
        );
        return Ok(());
    }

    // TRANSFER produces 2 events
    // 1. affected_account_id is sender, delta is negative, absolute_amount decreased
    // 2. affected_account_id is receiver, delta is positive, absolute_amount increased
    if method_name == "ft_transfer" || method_name == "ft_transfer_call" {
        let ft_transfer_args = match serde_json::from_slice::<FtTransfer>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => return Ok(()),
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };

        let delta = BigDecimal::from_str(&ft_transfer_args.amount)?;
        let negative_delta = delta.clone().mul(BigDecimal::from(-1));
        let memo = ft_transfer_args
            .memo
            .as_ref()
            .map(|s| s.escape_default().to_string());

        let base = legacy_ft::get_base(
            Event::TknNear,
            shard_id,
            events.len(),
            outcome,
            block_header,
        )?;
        let custom = legacy_ft::EventCustom {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: Some(ft_transfer_args.receiver_id.clone()),
            delta: negative_delta,
            cause: "TRANSFER".to_string(),
            memo: memo.clone(),
        };
        events.push(
            legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom).await?,
        );

        let base = legacy_ft::get_base(
            Event::TknNear,
            shard_id,
            events.len(),
            outcome,
            block_header,
        )?;
        let custom = legacy_ft::EventCustom {
            affected_id: ft_transfer_args.receiver_id,
            involved_id: Some(outcome.receipt.predecessor_id.clone()),
            delta,
            cause: "TRANSFER".to_string(),
            memo,
        };
        events.push(
            legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom).await?,
        );
        return Ok(());
    }

    // If TRANSFER failed, it could be revoked. The procedure is the same as for TRANSFER
    if method_name == "ft_resolve_transfer" {
        if outcome.execution_outcome.outcome.logs.is_empty() {
            // ft_transfer_call was successful, there's nothing to return back
            return Ok(());
        }
        let ft_refund_args = match serde_json::from_slice::<FtRefund>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => return Ok(()),
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };
        let delta = BigDecimal::from_str(&ft_refund_args.amount)?;
        let negative_delta = delta.clone().mul(BigDecimal::from(-1));
        let memo = ft_refund_args
            .memo
            .as_ref()
            .map(|s| s.escape_default().to_string());

        for log in &outcome.execution_outcome.outcome.logs {
            if log == "The account of the sender was deleted" {
                // I never met this case so it's better to re-check it manually when we find it
                tracing::error!(
                    target: crate::INDEXER,
                    "The account of the sender was deleted {}",
                    block_header.height
                );

                // we should revert ft_transfer_call, but there's no receiver_id. We should burn tokens
                let base = legacy_ft::get_base(
                    Event::TknNear,
                    shard_id,
                    events.len(),
                    outcome,
                    block_header,
                )?;
                let custom = legacy_ft::EventCustom {
                    affected_id: ft_refund_args.receiver_id,
                    involved_id: None,
                    delta: negative_delta,
                    cause: "BURN".to_string(),
                    memo,
                };
                events.push(
                    legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom)
                        .await?,
                );
                return Ok(());
            }
            if log.starts_with("Refund ") {
                // we should revert ft_transfer_call
                let base = legacy_ft::get_base(
                    Event::TknNear,
                    shard_id,
                    events.len(),
                    outcome,
                    block_header,
                )?;
                let custom = legacy_ft::EventCustom {
                    affected_id: ft_refund_args.receiver_id.clone(),
                    involved_id: Some(ft_refund_args.sender_id.clone()),
                    delta: negative_delta,
                    cause: "TRANSFER".to_string(),
                    memo: memo.clone(),
                };
                events.push(
                    legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom)
                        .await?,
                );

                let base = legacy_ft::get_base(
                    Event::TknNear,
                    shard_id,
                    events.len(),
                    outcome,
                    block_header,
                )?;
                let custom = legacy_ft::EventCustom {
                    affected_id: ft_refund_args.sender_id,
                    involved_id: Some(ft_refund_args.receiver_id),
                    delta,
                    cause: "TRANSFER".to_string(),
                    memo,
                };
                events.push(
                    legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom)
                        .await?,
                );
                return Ok(());
            }
        }
        return Ok(());
    }

    // BURN produces 1 event, where involved_account_id is NULL
    // I've seen no burn events, but if someone calls it, it should be like this
    if method_name == "near_withdraw" {
        let ft_burn_args = match serde_json::from_slice::<NearWithdraw>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => return Ok(()),
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };
        let negative_delta = BigDecimal::from_str(&ft_burn_args.amount)?.mul(BigDecimal::from(-1));

        let base = legacy_ft::get_base(
            Event::TknNear,
            shard_id,
            events.len(),
            outcome,
            block_header,
        )?;
        let custom = legacy_ft::EventCustom {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: None,
            delta: negative_delta,
            cause: "BURN".to_string(),
            memo: None,
        };
        events.push(
            legacy_ft::build_event(json_rpc_client, cache, block_header, base, custom).await?,
        );
        return Ok(());
    }

    tracing::error!(
        target: crate::INDEXER,
        "{} method {}",
        block_header.height,
        method_name
    );
    Ok(())
}
