use futures::FutureExt;
use mmb_utils::logger::print_info;
use mmb_utils::send_expected::SendExpected;
use std::panic;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Result;
use dashmap::DashMap;
use futures::future::join_all;
use itertools::Itertools;
use mmb_utils::cancellation_token::CancellationToken;
use tokio::sync::{broadcast, oneshot};
use tokio::time::Duration;

use crate::balance_manager::balance_manager::BalanceManager;
use crate::exchanges::block_reasons;
use crate::exchanges::common::ExchangeAccountId;
use crate::exchanges::events::{ExchangeEvent, ExchangeEvents};
use crate::exchanges::exchange_blocker::BlockType;
use crate::exchanges::exchange_blocker::ExchangeBlocker;
use crate::exchanges::general::exchange::Exchange;
use crate::exchanges::timeouts::timeout_manager::TimeoutManager;
use crate::lifecycle::shutdown::ShutdownService;
use crate::settings::CoreSettings;
use crate::{
    infrastructure::unset_lifetime_manager, lifecycle::app_lifetime_manager::AppLifetimeManager,
};
use parking_lot::Mutex;

use super::app_lifetime_manager::ActionAfterGracefulShutdown;
use super::launcher::unwrap_or_handle_panic;

pub trait Service: Send + Sync + 'static {
    fn name(&self) -> &str;

    fn graceful_shutdown(self: Arc<Self>) -> Option<oneshot::Receiver<Result<()>>>;
}

pub struct EngineContext {
    pub app_settings: CoreSettings,
    pub exchanges: DashMap<ExchangeAccountId, Arc<Exchange>>,
    pub shutdown_service: Arc<ShutdownService>,
    pub exchange_blocker: Arc<ExchangeBlocker>,
    pub lifetime_manager: Arc<AppLifetimeManager>,
    pub timeout_manager: Arc<TimeoutManager>,
    pub balance_manager: Arc<Mutex<BalanceManager>>,
    is_graceful_shutdown_started: AtomicBool,
    exchange_events: ExchangeEvents,
    finish_graceful_shutdown_sender: Mutex<Option<oneshot::Sender<ActionAfterGracefulShutdown>>>,
}

impl EngineContext {
    pub(crate) fn new(
        app_settings: CoreSettings,
        exchanges: DashMap<ExchangeAccountId, Arc<Exchange>>,
        exchange_events: ExchangeEvents,
        finish_graceful_shutdown_sender: oneshot::Sender<ActionAfterGracefulShutdown>,
        timeout_manager: Arc<TimeoutManager>,
        lifetime_manager: Arc<AppLifetimeManager>,
        balance_manager: Arc<Mutex<BalanceManager>>,
    ) -> Arc<Self> {
        let exchange_account_ids = app_settings
            .exchanges
            .iter()
            .map(|x| x.exchange_account_id)
            .collect_vec();

        let engine_context = Arc::new(EngineContext {
            app_settings,
            exchanges,
            shutdown_service: Default::default(),
            exchange_blocker: ExchangeBlocker::new(exchange_account_ids),
            lifetime_manager: lifetime_manager.clone(),
            timeout_manager,
            balance_manager,
            is_graceful_shutdown_started: Default::default(),
            exchange_events,
            finish_graceful_shutdown_sender: Mutex::new(Some(finish_graceful_shutdown_sender)),
        });

        lifetime_manager.setup_engine_context(engine_context.clone());

        engine_context
    }

    pub(crate) async fn graceful(
        self: Arc<Self>,
        action: ActionAfterGracefulShutdown,
        futures_cancellation_token: CancellationToken,
    ) {
        if self
            .is_graceful_shutdown_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        print_info("Graceful shutdown started");

        self.exchanges.iter().for_each(|x| {
            self.exchange_blocker.block(
                x.exchange_account_id,
                block_reasons::GRACEFUL_SHUTDOWN,
                BlockType::Manual,
            )
        });

        self.lifetime_manager.stop_token().cancel();

        self.shutdown_service.user_lvl_shutdown().await;
        self.exchange_blocker.stop_blocker().await;

        let cancellation_token = CancellationToken::default();
        const TIMEOUT: Duration = Duration::from_secs(5);

        tokio::select! {
            _ = cancel_opened_orders(&self.exchanges, cancellation_token.clone(), true) => (),
            _ = tokio::time::sleep(TIMEOUT) => {
                cancellation_token.cancel();
                log::error!(
                    "Timeout {} secs is exceeded: cancel open orders has been stopped",
                    TIMEOUT.as_secs(),
                );
            }
        }

        self.shutdown_service.core_lvl_shutdown().await;

        let disconnect_websockets = self
            .exchanges
            .iter()
            .map(|exchange| exchange.clone().disconnect());
        join_all(disconnect_websockets).await;

        self.finish_graceful_shutdown_sender
            .lock()
            .take()
            .expect("'finish_graceful_shutdown_sender' should exists in EngineContext")
            .send_expected(action);

        if let ActionAfterGracefulShutdown::Restart = action {
            futures_cancellation_token.cancel();
        }

        unset_lifetime_manager();

        print_info("Graceful shutdown finished");
    }

    pub fn get_events_channel(&self) -> broadcast::Receiver<ExchangeEvent> {
        self.exchange_events.get_events_channel()
    }
}

async fn cancel_opened_orders(
    exchanges: &DashMap<ExchangeAccountId, Arc<Exchange>>,
    cancellation_token: CancellationToken,
    add_missing_open_orders: bool,
) {
    log::info!("Canceling opened orders started");

    join_all(exchanges.iter().map(|x| {
        x.clone()
            .cancel_opened_orders(cancellation_token.clone(), add_missing_open_orders)
    }))
    .await;

    log::info!("Canceling opened orders finished");
}

pub struct TradingEngine {
    context: Arc<EngineContext>,
    finished_graceful_shutdown: oneshot::Receiver<ActionAfterGracefulShutdown>,
}

impl TradingEngine {
    pub fn new(
        context: Arc<EngineContext>,
        finished_graceful_shutdown: oneshot::Receiver<ActionAfterGracefulShutdown>,
    ) -> Self {
        TradingEngine {
            context,
            finished_graceful_shutdown,
        }
    }

    pub fn context(&self) -> Arc<EngineContext> {
        self.context.clone()
    }

    pub async fn run(self) -> ActionAfterGracefulShutdown {
        let action_outcome = AssertUnwindSafe(self.finished_graceful_shutdown)
            .catch_unwind()
            .await;

        let is_restart = unwrap_or_handle_panic(
            action_outcome,
            "Panic happened while TradingEngine was run",
            Some(self.context.lifetime_manager.clone()),
        )
        .expect("unwrap_or_handle_panic returned error")
        .expect("Failed to receive message from finished_graceful_shutdown");

        is_restart
    }
}
