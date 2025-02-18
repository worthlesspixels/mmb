use std::sync::Arc;

use super::commission::Commission;
use crate::exchanges::events::ExchangeEvent;
use crate::lifecycle::app_lifetime_manager::AppLifetimeManager;
use crate::lifecycle::launcher::EngineBuildConfig;
use crate::settings::ExchangeSettings;
use crate::{
    exchanges::{
        general::exchange::Exchange,
        timeouts::requests_timeout_manager_factory::RequestsTimeoutManagerFactory,
        timeouts::timeout_manager::TimeoutManager,
    },
    settings::CoreSettings,
};
use tokio::sync::broadcast;

pub fn create_timeout_manager(
    core_settings: &CoreSettings,
    build_settings: &EngineBuildConfig,
) -> Arc<TimeoutManager> {
    let request_timeout_managers = core_settings
        .exchanges
        .iter()
        .map(|exchange_settings| {
            let timeout_arguments = build_settings.supported_exchange_clients
                [&exchange_settings.exchange_account_id.exchange_id]
                .get_timeout_arguments();

            let exchange_account_id = exchange_settings.exchange_account_id;
            let request_timeout_manager = RequestsTimeoutManagerFactory::from_requests_per_period(
                timeout_arguments,
                exchange_account_id,
            );

            (exchange_account_id, request_timeout_manager)
        })
        .collect();

    TimeoutManager::new(request_timeout_managers)
}

pub async fn create_exchange(
    user_settings: &ExchangeSettings,
    build_settings: &EngineBuildConfig,
    events_channel: broadcast::Sender<ExchangeEvent>,
    lifetime_manager: Arc<AppLifetimeManager>,
    timeout_manager: Arc<TimeoutManager>,
) -> Arc<Exchange> {
    let exchange_client_builder =
        &build_settings.supported_exchange_clients[&user_settings.exchange_account_id.exchange_id];

    let exchange_client = exchange_client_builder.create_exchange_client(
        user_settings.clone(),
        events_channel.clone(),
        lifetime_manager.clone(),
    );

    let exchange = Exchange::new(
        user_settings.exchange_account_id,
        exchange_client.client,
        exchange_client.features,
        exchange_client_builder.get_timeout_arguments(),
        events_channel,
        lifetime_manager,
        timeout_manager,
        Commission::default(),
    );

    exchange.build_symbols(&user_settings.currency_pairs).await;

    exchange.clone().connect().await;

    exchange
}
