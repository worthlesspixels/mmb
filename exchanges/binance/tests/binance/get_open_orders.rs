use mmb_core::exchanges::common::*;
use mmb_core::exchanges::events::AllowedEventSourceType;
use mmb_core::exchanges::general::commission::Commission;
use mmb_core::exchanges::general::features::*;
use mmb_core::settings::{CurrencyPairSetting, ExchangeSettings};
use mmb_utils::cancellation_token::CancellationToken;
use mmb_utils::infrastructure::init_infrastructure;

use crate::binance::binance_builder::BinanceBuilder;
use crate::get_binance_credentials_or_exit;
use core_tests::order::OrderProxy;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_orders_exists() {
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

    let first_order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromOpenOrdersExistsTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );

    let second_order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromOpenOrdersExistsTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );

    if let Err(error) = first_order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
    {
        assert!(false, "Create order failed with error {:?}.", error)
    }

    if let Err(error) = second_order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
    {
        assert!(false, "Create order failed with error {:?}.", error)
    }

    let all_orders = binance_builder
        .exchange
        .clone()
        .get_open_orders(false)
        .await
        .expect("in test");

    let _ = binance_builder
        .exchange
        .cancel_opened_orders(CancellationToken::default(), true)
        .await;

    assert_eq!(all_orders.len(), 2);
}

/// It's matter to check branch for OneCurrencyPair variant
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_open_orders_for_each_currency_pair_separately() {
    init_infrastructure("log.txt");

    let exchange_account_id: ExchangeAccountId = "Binance_0".parse().expect("in test");
    let (api_key, secret_key) = get_binance_credentials_or_exit!();
    let mut settings =
        ExchangeSettings::new_short(exchange_account_id, api_key, secret_key, false, false);

    settings.currency_pairs = Some(vec![
        CurrencyPairSetting::Ordinary {
            base: "cnd".into(),
            quote: "btc".into(),
        },
        CurrencyPairSetting::Ordinary {
            base: "cnd".into(),
            quote: "btc".into(),
        },
    ]);

    let binance_builder = match BinanceBuilder::try_new_with_settings(
        settings.clone(),
        exchange_account_id,
        CancellationToken::default(),
        ExchangeFeatures::new(
            OpenOrdersType::OneCurrencyPair,
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

    let first_order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromGetOpenOrdersByCurrencyPairTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );

    first_order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("in test");

    let second_order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromGetOpenOrdersByCurrencyPairTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );

    second_order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("in test");

    let all_orders = binance_builder
        .exchange
        .get_open_orders(true)
        .await
        .expect("in test");

    let _ = binance_builder
        .exchange
        .cancel_opened_orders(CancellationToken::default(), true)
        .await;

    assert_eq!(all_orders.len(), 2);

    for order in all_orders {
        assert!(
            order.client_order_id == first_order_proxy.client_order_id
                || order.client_order_id == second_order_proxy.client_order_id
        );
    }
}
