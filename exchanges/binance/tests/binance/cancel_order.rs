use mmb_core::exchanges::common::*;
use mmb_core::exchanges::events::AllowedEventSourceType;
use mmb_core::exchanges::general::commission::Commission;
use mmb_core::exchanges::general::exchange::*;
use mmb_core::exchanges::general::features::*;
use mmb_core::orders::order::*;
use mmb_utils::cancellation_token::CancellationToken;
use mmb_utils::infrastructure::init_infrastructure;

use crate::binance::binance_builder::BinanceBuilder;
use core_tests::order::OrderProxy;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_successfully() {
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

    let order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromCancelledSuccessfullyTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );

    let order_ref = order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("Create order failed with error:");

    order_proxy
        .cancel_order_or_fail(&order_ref, binance_builder.exchange.clone())
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_opened_orders_successfully() {
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

    let first_order_proxy = OrderProxy::new(
        exchange_account_id,
        Some("FromCancelOpenedOrdersSuccessfullyTest".to_owned()),
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
        Some("FromCancelOpenedOrdersSuccessfullyTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );
    second_order_proxy
        .create_order(binance_builder.exchange.clone())
        .await
        .expect("in test");

    let orders = &binance_builder
        .exchange
        .get_open_orders(false)
        .await
        .expect("Opened orders not found for exchange account id:");

    assert_ne!(orders.len(), 0);
    binance_builder
        .exchange
        .clone()
        .cancel_opened_orders(CancellationToken::default(), true)
        .await;

    let orders = &binance_builder
        .exchange
        .get_open_orders(false)
        .await
        .expect("Opened orders not found for exchange account id");

    assert_eq!(orders.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_to_cancel() {
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

    let order = OrderProxy::new(
        exchange_account_id,
        Some("FromNothingToCancelTest".to_owned()),
        CancellationToken::default(),
        binance_builder.default_price,
        binance_builder.min_amount,
    );
    let order_to_cancel = OrderCancelling {
        header: order.make_header(),
        exchange_order_id: "1234567890".into(),
    };

    // Cancel last order
    let cancel_outcome = binance_builder
        .exchange
        .cancel_order(&order_to_cancel, CancellationToken::default())
        .await
        .expect("in test");
    if let RequestResult::Error(error) = cancel_outcome.outcome {
        assert_eq!(error.error_type, ExchangeErrorType::OrderNotFound);
    }
}
