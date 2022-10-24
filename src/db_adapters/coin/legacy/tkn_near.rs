use crate::db_adapters;
use crate::db_adapters::{coin, contracts, numeric_types, Event};
use crate::models::coin_events::CoinEvent;
use bigdecimal::BigDecimal;
use near_lake_framework::near_indexer_primitives;
use near_primitives::types::AccountId;
use near_primitives::views::{ActionView, ExecutionStatusView, ReceiptEnumView};
use serde::Deserialize;
use std::ops::{Mul, Sub};
use std::str::FromStr;

#[derive(Deserialize, Debug, Clone)]
struct FtNew {
    // pub metadata: ...,
    pub owner_id: AccountId,
    pub total_supply: numeric_types::U128,
}

#[derive(Deserialize, Debug, Clone)]
struct FtTransfer {
    pub receiver_id: AccountId,
    pub amount: numeric_types::U128,
    pub memo: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct FtRefund {
    pub receiver_id: AccountId,
    pub sender_id: AccountId,
    pub amount: numeric_types::U128,
    pub memo: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
struct NearWithdraw {
    pub amount: numeric_types::U128,
}

pub(crate) async fn collect_tkn_near(
    json_rpc_client: &near_jsonrpc_client::JsonRpcClient,
    shard_id: &near_indexer_primitives::types::ShardId,
    receipt_execution_outcomes: &[near_indexer_primitives::IndexerExecutionOutcomeWithReceipt],
    block_header: &near_indexer_primitives::views::BlockHeaderView,
    ft_balance_cache: &crate::FtBalanceCache,
    contracts: &contracts::ContractsHelper,
) -> anyhow::Result<Vec<CoinEvent>> {
    let mut events: Vec<CoinEvent> = vec![];

    for outcome in receipt_execution_outcomes {
        if !is_tkn_near_contract(outcome.receipt.receiver_id.as_str())
            || !db_adapters::events::extract_events(outcome).is_empty()
            || contracts
                .is_contract_inconsistent(&outcome.receipt.receiver_id)
                .await
        {
            continue;
        }
        if let ReceiptEnumView::Action { actions, .. } = &outcome.receipt.receipt {
            for action in actions {
                events.extend(
                    process_tkn_near_functions(
                        json_rpc_client,
                        block_header,
                        ft_balance_cache,
                        action,
                        outcome,
                        contracts,
                    )
                    .await?,
                );
            }
        }
    }
    if !events.is_empty() {
        coin::register_new_contracts(&mut events, contracts).await?;
        coin::filter_inconsistent_events(&mut events, json_rpc_client, block_header, contracts)
            .await?;
        coin::enumerate_events(
            &mut events,
            shard_id,
            block_header.timestamp,
            &Event::TknNear,
        )?;
    }
    Ok(events)
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
    json_rpc_client: &near_jsonrpc_client::JsonRpcClient,
    block_header: &near_indexer_primitives::views::BlockHeaderView,
    cache: &crate::FtBalanceCache,
    action: &ActionView,
    outcome: &near_indexer_primitives::IndexerExecutionOutcomeWithReceipt,
    contracts: &contracts::ContractsHelper,
) -> anyhow::Result<Vec<CoinEvent>> {
    let (method_name, args, deposit) = match action {
        ActionView::FunctionCall {
            method_name,
            args,
            deposit,
            ..
        } => (method_name, args, deposit),
        _ => return Ok(vec![]),
    };

    let decoded_args = base64::decode(args)?;

    if method_name == "storage_deposit" {
        return Ok(vec![]);
    }

    // may mint the tokens
    if method_name == "new" {
        let args = match serde_json::from_slice::<FtNew>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => {
                        return Ok(vec![])
                    }
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };

        // We need to fill in the cache here because at the end of the block we are checking all the balances.
        // If the cache is empty, we query the state at the previous block.
        // For `new` method, the previous block may not have the contract deployed
        let account_with_contract = crate::AccountWithContract {
            account_id: args.owner_id.clone(),
            contract_account_id: outcome.execution_outcome.outcome.executor_id.clone(),
        };
        coin::balance_utils::save_latest_balance(account_with_contract, 0, cache).await;

        let delta = BigDecimal::from_str(&args.total_supply.0.to_string())?;
        let base = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
        let custom = coin::FtEvent {
            affected_id: args.owner_id,
            involved_id: None,
            delta,
            cause: "MINT".to_string(),
            memo: None,
        };
        return Ok(vec![
            coin::build_event(
                json_rpc_client,
                cache,
                block_header,
                base,
                custom,
                contracts,
            )
            .await?,
        ]);
    }

    // MINT produces 1 event, where involved_account_id is NULL.
    if method_name == "near_deposit" {
        let delta = BigDecimal::from_str(&deposit.to_string())?;
        let base = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
        let custom = coin::FtEvent {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: None,
            delta,
            cause: "MINT".to_string(),
            memo: None,
        };
        return Ok(vec![
            coin::build_event(
                json_rpc_client,
                cache,
                block_header,
                base,
                custom,
                contracts,
            )
            .await?,
        ]);
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
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => {
                        return Ok(vec![])
                    }
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };

        let delta = BigDecimal::from_str(&ft_transfer_args.amount.0.to_string())?;
        let negative_delta = delta.clone().mul(BigDecimal::from(-1));
        let memo = ft_transfer_args
            .memo
            .as_ref()
            .map(|s| s.escape_default().to_string());

        let base_from = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
        let custom_from = coin::FtEvent {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: Some(ft_transfer_args.receiver_id.clone()),
            delta: negative_delta,
            cause: "TRANSFER".to_string(),
            memo: memo.clone(),
        };

        let base_to = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
        let custom_to = coin::FtEvent {
            affected_id: ft_transfer_args.receiver_id,
            involved_id: Some(outcome.receipt.predecessor_id.clone()),
            delta,
            cause: "TRANSFER".to_string(),
            memo,
        };
        return Ok(vec![
            coin::build_event(
                json_rpc_client,
                cache,
                block_header,
                base_from,
                custom_from,
                contracts,
            )
            .await?,
            coin::build_event(
                json_rpc_client,
                cache,
                block_header,
                base_to,
                custom_to,
                contracts,
            )
            .await?,
        ]);
    }

    // If TRANSFER failed, it could be revoked. The procedure is the same as for TRANSFER
    if method_name == "ft_resolve_transfer" {
        if outcome.execution_outcome.outcome.logs.is_empty() {
            // ft_transfer_call was successful, there's nothing to return back
            return Ok(vec![]);
        }
        let ft_refund_args = match serde_json::from_slice::<FtRefund>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => {
                        return Ok(vec![])
                    }
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };
        let mut delta = BigDecimal::from_str(&ft_refund_args.amount.0.to_string())?;
        // The contract may return only the part of the coins.
        // We should parse it from the output and subtract from the value from args
        if let ExecutionStatusView::SuccessValue(transferred_amount_decoded) =
            &outcome.execution_outcome.outcome.status
        {
            let transferred_amount =
                serde_json::from_slice::<String>(&base64::decode(transferred_amount_decoded)?)?;
            delta = delta.sub(BigDecimal::from_str(&transferred_amount)?);
        }
        let negative_delta = delta.clone().mul(BigDecimal::from(-1));
        let memo = ft_refund_args
            .memo
            .as_ref()
            .map(|s| s.escape_default().to_string());

        for log in &outcome.execution_outcome.outcome.logs {
            if log == "The account of the sender was deleted" {
                // I never met this case so it's better to re-check it manually when we find it
                tracing::error!(
                    target: crate::LOGGING_PREFIX,
                    "The account of the sender was deleted {}",
                    block_header.height
                );

                // we should revert ft_transfer_call, but there's no receiver_id. We should burn tokens
                let base = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
                let custom = coin::FtEvent {
                    affected_id: ft_refund_args.receiver_id,
                    involved_id: None,
                    delta: negative_delta,
                    cause: "BURN".to_string(),
                    memo,
                };
                return Ok(vec![
                    coin::build_event(
                        json_rpc_client,
                        cache,
                        block_header,
                        base,
                        custom,
                        contracts,
                    )
                    .await?,
                ]);
            }
            if log.starts_with("Refund ") {
                // we should revert ft_transfer_call
                let base_from = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
                let custom_from = coin::FtEvent {
                    affected_id: ft_refund_args.receiver_id.clone(),
                    involved_id: Some(ft_refund_args.sender_id.clone()),
                    delta: negative_delta,
                    cause: "TRANSFER".to_string(),
                    memo: memo.clone(),
                };

                let base_to = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
                let custom_to = coin::FtEvent {
                    affected_id: ft_refund_args.sender_id,
                    involved_id: Some(ft_refund_args.receiver_id),
                    delta,
                    cause: "TRANSFER".to_string(),
                    memo,
                };

                return Ok(vec![
                    coin::build_event(
                        json_rpc_client,
                        cache,
                        block_header,
                        base_from,
                        custom_from,
                        contracts,
                    )
                    .await?,
                    coin::build_event(
                        json_rpc_client,
                        cache,
                        block_header,
                        base_to,
                        custom_to,
                        contracts,
                    )
                    .await?,
                ]);
            }
        }
        return Ok(vec![]);
    }

    // BURN produces 1 event, where involved_account_id is NULL
    // I've seen no burn events, but if someone calls it, it should be like this
    if method_name == "near_withdraw" {
        let ft_burn_args = match serde_json::from_slice::<NearWithdraw>(&decoded_args) {
            Ok(x) => x,
            Err(err) => {
                match outcome.execution_outcome.outcome.status {
                    // We couldn't parse args for failed receipt. Let's just ignore it, we can't save it properly
                    ExecutionStatusView::Unknown | ExecutionStatusView::Failure(_) => {
                        return Ok(vec![])
                    }
                    ExecutionStatusView::SuccessValue(_)
                    | ExecutionStatusView::SuccessReceiptId(_) => {
                        anyhow::bail!(err)
                    }
                }
            }
        };
        let negative_delta =
            BigDecimal::from_str(&ft_burn_args.amount.0.to_string())?.mul(BigDecimal::from(-1));

        let base = db_adapters::get_base(Event::TknNear, outcome, block_header)?;
        let custom = coin::FtEvent {
            affected_id: outcome.receipt.predecessor_id.clone(),
            involved_id: None,
            delta: negative_delta,
            cause: "BURN".to_string(),
            memo: None,
        };
        return Ok(vec![
            coin::build_event(
                json_rpc_client,
                cache,
                block_header,
                base,
                custom,
                contracts,
            )
            .await?,
        ]);
    }

    tracing::error!(
        target: crate::LOGGING_PREFIX,
        "TKN {} method {}",
        block_header.height,
        method_name
    );
    Ok(vec![])
}
