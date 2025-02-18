use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use mmb_utils::DateTime;
use tokio::sync::broadcast;

use super::{
    common::CurrencyCode,
    common::{
        ActivePosition, CurrencyPair, ExchangeAccountId, ExchangeError, RestRequestOutcome,
        SpecificCurrencyPair,
    },
    common::{Amount, ClosedPosition, CurrencyId, Price},
    events::{ExchangeBalancesAndPositions, TradeId},
    general::handlers::handle_order_filled::FillEventData,
    general::symbol::BeforeAfter,
    general::{order::get_order_trades::OrderTrade, symbol::Symbol},
    timeouts::requests_timeout_manager_factory::RequestTimeoutArguments,
};
use crate::exchanges::events::ExchangeEvent;
use crate::exchanges::general::features::ExchangeFeatures;
use crate::lifecycle::app_lifetime_manager::AppLifetimeManager;
use crate::orders::fill::EventSourceType;
use crate::orders::order::{
    ClientOrderId, ExchangeOrderId, OrderCancelling, OrderCreating, OrderInfo,
};
use crate::settings::ExchangeSettings;
use crate::{connectivity::connectivity_manager::WebSocketRole, orders::order::OrderSide};
use crate::{exchanges::general::exchange::BoxExchangeClient, orders::pool::OrderRef};
use url::Url;

// Implementation of rest API client
#[async_trait]
pub trait ExchangeClient: Support {
    async fn request_all_symbols(&self) -> Result<RestRequestOutcome>;

    async fn create_order(&self, order: &OrderCreating) -> Result<RestRequestOutcome>;

    async fn request_cancel_order(&self, order: &OrderCancelling) -> Result<RestRequestOutcome>;

    async fn cancel_all_orders(&self, currency_pair: CurrencyPair) -> Result<()>;

    async fn get_open_orders(&self) -> Result<Vec<OrderInfo>>;

    async fn get_open_orders_by_currency_pair(
        &self,
        currency_pair: CurrencyPair,
    ) -> Result<Vec<OrderInfo>>;

    async fn get_order_info(&self, order: &OrderRef) -> Result<OrderInfo, ExchangeError>;

    async fn request_my_trades(
        &self,
        symbol: &Symbol,
        last_date_time: Option<DateTime>,
    ) -> Result<RestRequestOutcome>;

    async fn request_get_position(&self) -> Result<RestRequestOutcome>;

    async fn request_get_balance_and_position(&self) -> Result<RestRequestOutcome>;

    async fn get_balance(&self) -> Result<ExchangeBalancesAndPositions>;

    async fn request_close_position(
        &self,
        position: &ActivePosition,
        price: Option<Price>,
    ) -> Result<RestRequestOutcome>;
}

#[async_trait]
pub trait Support: Send + Sync {
    fn get_order_id(&self, response: &RestRequestOutcome) -> Result<ExchangeOrderId>;

    fn on_websocket_message(&self, msg: &str) -> Result<()>;
    fn on_connecting(&self) -> Result<()>;

    fn set_order_created_callback(
        &self,
        callback: Box<dyn FnMut(ClientOrderId, ExchangeOrderId, EventSourceType) + Send + Sync>,
    );

    fn set_order_cancelled_callback(
        &self,
        callback: Box<dyn FnMut(ClientOrderId, ExchangeOrderId, EventSourceType) + Send + Sync>,
    );

    fn set_handle_order_filled_callback(
        &self,
        callback: Box<dyn FnMut(FillEventData) + Send + Sync>,
    );

    fn set_handle_trade_callback(
        &self,
        callback: Box<
            dyn FnMut(CurrencyPair, TradeId, Price, Amount, OrderSide, DateTime) + Send + Sync,
        >,
    );

    fn set_traded_specific_currencies(&self, currencies: Vec<SpecificCurrencyPair>);

    fn is_websocket_enabled(&self, role: WebSocketRole) -> bool;

    async fn create_ws_url(&self, role: WebSocketRole) -> Result<Url>;

    fn get_specific_currency_pair(&self, currency_pair: CurrencyPair) -> SpecificCurrencyPair;

    fn get_supported_currencies(&self) -> &DashMap<CurrencyId, CurrencyCode>;

    fn should_log_message(&self, message: &str) -> bool;

    fn log_unknown_message(&self, exchange_account_id: ExchangeAccountId, message: &str) {
        log::info!("Unknown message for {}: {}", exchange_account_id, message);
    }

    fn parse_all_symbols(&self, response: &RestRequestOutcome) -> Result<Vec<Arc<Symbol>>>;

    fn get_balance_reservation_currency_code(
        &self,
        symbol: Arc<Symbol>,
        side: OrderSide,
    ) -> CurrencyCode {
        symbol.get_trade_code(side, BeforeAfter::Before)
    }

    fn parse_get_my_trades(
        &self,
        response: &RestRequestOutcome,
        last_date_time: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<OrderTrade>>;

    fn get_settings(&self) -> &ExchangeSettings;

    fn parse_get_position(&self, response: &RestRequestOutcome) -> Vec<ActivePosition>;

    fn parse_close_position(&self, response: &RestRequestOutcome) -> Result<ClosedPosition>;

    fn parse_get_balance(&self, response: &RestRequestOutcome) -> ExchangeBalancesAndPositions;
}

pub struct ExchangeClientBuilderResult {
    pub client: BoxExchangeClient,
    pub features: ExchangeFeatures,
}

pub trait ExchangeClientBuilder {
    fn create_exchange_client(
        &self,
        exchange_settings: ExchangeSettings,
        events_channel: broadcast::Sender<ExchangeEvent>,
        lifetime_manager: Arc<AppLifetimeManager>,
    ) -> ExchangeClientBuilderResult;

    fn get_timeout_arguments(&self) -> RequestTimeoutArguments;
}
