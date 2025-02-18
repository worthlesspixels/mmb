use std::{sync::Arc, time::Duration};

use futures::FutureExt;
use mmb_utils::{
    cancellation_token::CancellationToken,
    infrastructure::{FutureOutcome, SpawnFutureFlags},
};
use mockall_double::double;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

#[double]
use crate::balance_manager::balance_manager::BalanceManager;
#[double]
use crate::exchanges::general::engine_api::EngineApi;

use crate::{
    exchanges::common::MarketAccountId, infrastructure::spawn_future_timed,
    orders::order::OrderSide,
};

pub fn close_position_if_needed(
    market_account_id: &MarketAccountId,
    balance_manager: Option<Arc<Mutex<BalanceManager>>>,
    engine_api: Arc<EngineApi>,
    cancellation_token: CancellationToken,
) -> Option<JoinHandle<FutureOutcome>> {
    match balance_manager {
        Some(balance_manager) => {
            if balance_manager
                .lock()
                .get_position(
                    market_account_id.exchange_account_id,
                    market_account_id.currency_pair,
                    OrderSide::Buy,
                )
                .is_zero()
            {
                return None;
            }
        }
        None => return None,
    }

    let action = async move {
        log::info!("Started closing active positions");
        engine_api.close_active_positions(cancellation_token).await;
        log::info!("Finished closing active positions");
        Ok(())
    };

    let action_name = "Close active positions";
    Some(spawn_future_timed(
        action_name,
        SpawnFutureFlags::STOP_BY_TOKEN | SpawnFutureFlags::CRITICAL,
        Duration::from_secs(30),
        action.boxed(),
    ))
}
