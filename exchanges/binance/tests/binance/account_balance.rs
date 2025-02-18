use mmb_core::exchanges::{
    common::ExchangeAccountId,
    events::AllowedEventSourceType,
    general::{
        commission::Commission,
        features::{
            ExchangeFeatures, OpenOrdersType, OrderFeatures, OrderTradeOption, RestFillsFeatures,
            WebSocketOptions,
        },
    },
};
use mmb_utils::{cancellation_token::CancellationToken, infrastructure::init_infrastructure};

use super::binance_builder::BinanceBuilder;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_balance_successfully() {
    init_infrastructure("log.txt");

    let exchange_account_id: ExchangeAccountId = "Binance_0".parse().expect("in test");
    let binance_builder = match BinanceBuilder::try_new(
        exchange_account_id,
        CancellationToken::default(),
        ExchangeFeatures::new(
            OpenOrdersType::AllCurrencyPair,
            RestFillsFeatures::default(),
            OrderFeatures::default(),
            OrderTradeOption::default(),
            WebSocketOptions::default(),
            true,
            true,
            AllowedEventSourceType::default(),
            AllowedEventSourceType::default(),
        ),
        Commission::default(),
        true,
    )
    .await
    {
        Ok(binance_builder) => binance_builder,
        Err(_) => return,
    };

    let result = binance_builder
        .exchange
        .get_balance(CancellationToken::default())
        .await;

    log::info!("Balance: {:?}", result);

    assert!(result.is_some());
}
