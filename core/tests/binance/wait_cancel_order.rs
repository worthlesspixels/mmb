use mmb_core::core::exchanges::common::*;
use mmb_core::core::exchanges::events::AllowedEventSourceType;
use mmb_core::core::exchanges::general::commission::Commission;
use mmb_core::core::exchanges::general::features::*;
use mmb_core::core::settings::{CurrencyPairSetting, ExchangeSettings};
use mmb_utils::cancellation_token::CancellationToken;
use mmb_utils::logger::init_logger;

use crate::binance::binance_builder::BinanceBuilder;
use crate::core::order::OrderProxy;
use crate::get_binance_credentials_or_exit;

#[tokio::test]
async fn cancellation_waited_successfully() {
    init_logger();

    let exchange_account_id: ExchangeAccountId = "Binance_0".parse().expect("in test");
    let (api_key, secret_key) = get_binance_credentials_or_exit!();
    let mut settings = ExchangeSettings::new_short(exchange_account_id, api_key, secret_key, false);

    // Currency pair in settings are matter here because of need to check
    // Symbol in check_order_fills() inside wait_cancel_order()
    settings.currency_pairs = Some(vec![CurrencyPairSetting {
        base: "cnd".into(),
        quote: "btc".into(),
        currency_pair: None,
    }]);

    let binance_builder = match BinanceBuilder::try_new_with_settings(
        settings.clone(),
        exchange_account_id,
        CancellationToken::default(),
        ExchangeFeatures::new(
            OpenOrdersType::AllCurrencyPair,
            RestFillsFeatures::default(),
            OrderFeatures::default(),
            OrderTradeOption::default(),
            WebSocketOptions::default(),
            false,
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

    let order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromCancellationWaitedSuccessfullyTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
    );

    let order_ref = order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("Create order failed with error");

    // If here are no error - order was cancelled successfully
    binance_builder
        .exchange
        .wait_cancel_order(order_ref, None, true, CancellationToken::new())
        .await
        .expect("Error while trying wait_cancel_order");
}

#[tokio::test]
async fn cancellation_waited_failed_fallback() {
    init_logger();

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
            false,
            true,
            AllowedEventSourceType::default(),
            AllowedEventSourceType::FallbackOnly,
        ),
        Commission::default(),
        true,
    )
    .await
    {
        Ok(binance_builder) => binance_builder,
        Err(_) => return,
    };

    let order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromCancellationWaitedFailedFallbackTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
    );

    let order_ref = order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("Create order failed with error");

    let error = binance_builder
        .exchange
        .wait_cancel_order(order_ref, None, true, CancellationToken::new())
        .await
        .err()
        .expect("Error was expected while trying wait_cancel_order()");

    assert_eq!(
        "Order was expected to cancel explicitly via Rest or Web Socket but got timeout instead",
        &error.to_string()[..86]
    );
}
