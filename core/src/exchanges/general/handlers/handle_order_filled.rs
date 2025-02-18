use chrono::Utc;
use mmb_utils::infrastructure::WithExpect;
use mmb_utils::DateTime;
use parking_lot::RwLock;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use uuid::Uuid;

use crate::{
    exchanges::{
        common::Amount,
        common::CurrencyCode,
        common::CurrencyPair,
        common::ExchangeAccountId,
        common::Price,
        events::{AllowedEventSourceType, TradeId},
        general::commission::Percent,
        general::exchange::Exchange,
        general::symbol::{Round, Symbol},
    },
    math::ConvertPercentToRate,
    orders::{
        event::OrderEventType,
        fill::EventSourceType,
        fill::OrderFill,
        fill::OrderFillType,
        order::ClientOrderId,
        order::ExchangeOrderId,
        order::OrderSide,
        order::OrderSnapshot,
        order::OrderStatus,
        order::OrderType,
        order::{ClientOrderFillId, OrderRole},
        pool::OrderRef,
    },
};

type ArgsToLog = (
    ExchangeAccountId,
    Option<TradeId>,
    Option<ClientOrderId>,
    ExchangeOrderId,
    AllowedEventSourceType,
    EventSourceType,
);

#[derive(Debug, Clone)]
pub struct FillEventData {
    pub source_type: EventSourceType,
    pub trade_id: Option<TradeId>,
    pub client_order_id: Option<ClientOrderId>,
    pub exchange_order_id: ExchangeOrderId,
    pub fill_price: Price,
    pub fill_amount: Amount,
    pub is_diff: bool,
    pub total_filled_amount: Option<Amount>,
    pub order_role: Option<OrderRole>,
    pub commission_currency_code: Option<CurrencyCode>,
    pub commission_rate: Option<Percent>,
    pub commission_amount: Option<Amount>,
    pub fill_type: OrderFillType,
    pub trade_currency_pair: Option<CurrencyPair>,
    pub order_side: Option<OrderSide>,
    pub order_amount: Option<Amount>,
    pub fill_date: Option<DateTime>,
}

impl Exchange {
    pub fn handle_order_filled(&self, mut event_data: FillEventData) {
        let args_to_log = (
            self.exchange_account_id,
            event_data.trade_id.clone(),
            event_data.client_order_id.clone(),
            event_data.exchange_order_id.clone(),
            self.features.allowed_fill_event_source_type,
            event_data.source_type,
        );

        if Self::should_ignore_event(
            self.features.allowed_fill_event_source_type,
            event_data.source_type,
        ) {
            log::info!("Ignoring fill {:?}", args_to_log);
            return;
        }

        if event_data.exchange_order_id.is_empty() {
            panic!(
                "Received HandleOrderFilled with an empty exchangeOrderId {:?}",
                &args_to_log
            );
        }

        self.add_external_order(&mut event_data, &args_to_log);

        match self
            .orders
            .cache_by_exchange_id
            .get(&event_data.exchange_order_id)
        {
            None => {
                if let Some(client_order_id) = &event_data.client_order_id {
                    self.handle_create_order_succeeded(
                        self.exchange_account_id,
                        client_order_id,
                        &event_data.exchange_order_id,
                        &event_data.source_type,
                    )
                    .with_expect(|| {
                        format!("Error handle create order succeeded for clientOrderId {client_order_id}")
                    });

                    let order_ref = self.orders.cache_by_exchange_id.get(&event_data.exchange_order_id).
                        expect("Order should be inserted in orders.cache_by_exchange_id by handle_create_order_succeeded called above");
                    return self.create_and_add_order_fill(&mut event_data, &order_ref);
                }

                log::info!("Received a fill for not existing order {:?}", &args_to_log);

                let source_type = event_data.source_type;
                let exchange_order_id = event_data.exchange_order_id.clone();
                let client_or_order_id = event_data.client_order_id.clone();

                self.buffered_fills_manager
                    .lock()
                    .add_fill(self.exchange_account_id, event_data);

                if let Some(client_order_id) = client_or_order_id {
                    self.raise_order_created(&client_order_id, &exchange_order_id, source_type);
                }
            }
            Some(order_ref) => self.create_and_add_order_fill(&mut event_data, &order_ref),
        }
    }

    fn was_trade_already_received(
        trade_id: &Option<TradeId>,
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
    ) -> bool {
        let current_trade_id = match trade_id {
            None => return false,
            Some(trade_id) => trade_id,
        };

        if order_fills.iter().any(|fill| {
            fill.trade_id()
                .map(|fill_trade_id| fill_trade_id == current_trade_id)
                .unwrap_or(false)
        }) {
            log::info!(
                "Trade with {} was received already for order {:?}",
                current_trade_id,
                order_ref
            );

            return true;
        }

        false
    }

    fn diff_fill_after_non_diff(
        event_data: &FillEventData,
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
    ) -> bool {
        if event_data.is_diff && order_fills.iter().any(|fill| !fill.is_diff()) {
            // Most likely we received a trade update (diff), then received a non-diff fill via fallback and then again received a diff trade update
            // It happens when WebSocket is glitchy and we miss update and the problem is we have no idea how to handle diff updates
            // after applying a non-diff one as there's no TradeId, so we have to ignore all the diff updates afterwards
            // relying only on fallbacks
            log::warn!(
                "Unable to process a diff fill after a non-diff one {:?}",
                order_ref
            );

            return true;
        }

        false
    }

    fn filled_amount_not_less_event_fill(
        event_data: &FillEventData,
        order_filled_amount: Amount,
        order_ref: &OrderRef,
    ) -> bool {
        if !event_data.is_diff && order_filled_amount >= event_data.fill_amount {
            log::warn!(
                "order.filled_amount is {} >= received fill {}, so non-diff fill for {} {:?} should be ignored",
                order_filled_amount,
                event_data.fill_amount,
                order_ref.client_order_id(),
                order_ref.exchange_order_id(),
            );

            return true;
        }

        false
    }

    fn should_miss_fill(
        event_data: &FillEventData,
        order_filled_amount: Amount,
        last_fill_amount: Amount,
        order_ref: &OrderRef,
    ) -> bool {
        if let Some(total_filled_amount) = event_data.total_filled_amount {
            if order_filled_amount + last_fill_amount != total_filled_amount {
                log::warn!(
                    "Fill was missed because {} != {} for {:?}",
                    order_filled_amount,
                    total_filled_amount,
                    order_ref
                );

                return true;
            }
        }

        false
    }

    fn get_last_fill_data(
        event_data: &mut FillEventData,
        symbol: &Symbol,
        order_fills: &Vec<OrderFill>,
        order_filled_amount: Amount,
        order_ref: &OrderRef,
    ) -> Option<(Price, Amount, Price)> {
        let mut last_fill_amount = event_data.fill_amount;
        let mut last_fill_price = event_data.fill_price;
        let mut last_fill_cost = if !symbol.is_derivative() {
            last_fill_amount * last_fill_price
        } else {
            last_fill_amount / last_fill_price
        };

        if !event_data.is_diff && order_fills.len() > 0 {
            match Self::calculate_cost_diff(&order_fills, order_ref, last_fill_cost) {
                None => return None,
                Some(cost_diff) => {
                    let (price, amount, cost) = Self::calculate_last_fill_data(
                        last_fill_amount,
                        order_filled_amount,
                        &symbol,
                        cost_diff,
                    );
                    last_fill_price = price;
                    last_fill_amount = amount;
                    last_fill_cost = cost;

                    Self::set_commission_amount(event_data, order_fills);
                }
            };
        }

        if last_fill_amount.is_zero() {
            log::warn!(
                "last_fill_amount was received for 0 for {}, {:?}",
                order_ref.client_order_id(),
                order_ref.exchange_order_id()
            );

            return None;
        }

        Some((last_fill_price, last_fill_amount, last_fill_cost))
    }

    fn calculate_cost_diff(
        order_fills: &Vec<OrderFill>,
        order_ref: &OrderRef,
        last_fill_cost: Decimal,
    ) -> Option<Decimal> {
        // Diff should be calculated only if it is not the first fill
        let total_filled_cost: Decimal = order_fills.iter().map(|fill| fill.cost()).sum();
        let cost_diff = last_fill_cost - total_filled_cost;
        if cost_diff <= dec!(0) {
            log::warn!(
                "cost_diff is {} which is <= 0 for {:?}",
                cost_diff,
                order_ref
            );

            return None;
        }

        Some(cost_diff)
    }

    fn calculate_last_fill_data(
        last_fill_amount: Amount,
        order_filled_amount: Amount,
        symbol: &Symbol,
        cost_diff: Price,
    ) -> (Price, Amount, Price) {
        let amount_diff = last_fill_amount - order_filled_amount;
        let res_fill_price = if !symbol.is_derivative() {
            cost_diff / amount_diff
        } else {
            amount_diff / cost_diff
        };
        let last_fill_price = symbol.price_round(res_fill_price, Round::ToNearest);

        let last_fill_amount = amount_diff;
        let last_fill_cost = cost_diff;

        (last_fill_price, last_fill_amount, last_fill_cost)
    }

    fn set_commission_amount(event_data: &mut FillEventData, order_fills: &Vec<OrderFill>) {
        if let Some(commission_amount) = event_data.commission_amount {
            let current_commission: Decimal = order_fills
                .iter()
                .map(|fill| fill.commission_amount())
                .sum();
            event_data.commission_amount = Some(commission_amount - current_commission);
        }
    }

    fn panic_if_wrong_status_or_cancelled(order_ref: &OrderRef, event_data: &FillEventData) {
        if order_ref.status() == OrderStatus::FailedToCreate
            || order_ref.status() == OrderStatus::Completed
            || order_ref.was_cancellation_event_raised()
        {
            panic!(
                "Fill was received for a {:?} {} {:?}",
                order_ref.status(),
                order_ref.was_cancellation_event_raised(),
                event_data
            );
        }
    }

    fn get_order_role(event_data: &FillEventData, order_ref: &OrderRef) -> OrderRole {
        match &event_data.order_role {
            Some(order_role) => order_role.clone(),
            None => {
                if event_data.commission_amount.is_none()
                    && event_data.commission_rate.is_none()
                    && order_ref.role().is_none()
                {
                    panic!("Fill has neither commission nor commission rate");
                }

                order_ref.role().expect("Unable to determine order_role")
            }
        }
    }

    fn get_commission_amount(
        event_data_commission_amount: Option<Amount>,
        event_data_commission_rate: Option<Decimal>,
        expected_commission_rate: Percent,
        last_fill_amount: Amount,
        last_fill_price: Price,
        commission_currency_code: CurrencyCode,
        symbol: &Symbol,
    ) -> Amount {
        match event_data_commission_amount {
            Some(commission_amount) => commission_amount.clone(),
            None => {
                let commission_rate = match event_data_commission_rate {
                    Some(commission_rate) => commission_rate.clone(),
                    None => expected_commission_rate,
                };

                let last_fill_amount_in_currency_code = symbol
                    .convert_amount_from_amount_currency_code(
                        commission_currency_code,
                        last_fill_amount,
                        last_fill_price,
                    );
                last_fill_amount_in_currency_code * commission_rate
            }
        }
    }

    fn set_commission_rate(
        &self,
        event_data: &mut FillEventData,
        order_role: OrderRole,
    ) -> Decimal {
        let commission = self.commission.get_commission(order_role).fee;
        let expected_commission_rate = commission.percent_to_rate();

        if event_data.commission_amount.is_none() && event_data.commission_rate.is_none() {
            event_data.commission_rate = Some(expected_commission_rate);
        }

        expected_commission_rate
    }

    fn update_commission_for_bnb_case(
        &self,
        commission_currency_code: CurrencyCode,
        symbol: &Symbol,
        commission_amount: Amount,
        converted_commission_amount: &mut Amount,
        converted_commission_currency_code: &mut CurrencyCode,
    ) {
        if commission_currency_code != symbol.base_currency_code()
            && commission_currency_code != symbol.quote_currency_code()
        {
            let mut currency_pair =
                CurrencyPair::from_codes(commission_currency_code, symbol.quote_currency_code());
            match self.order_book_top.get(&currency_pair) {
                Some(top_prices) => {
                    let bid = top_prices
                        .bid
                        .as_ref()
                        .expect("There are no top bid in order book");
                    let price_bnb_quote = bid.price;
                    *converted_commission_amount = commission_amount * price_bnb_quote;
                    *converted_commission_currency_code = symbol.quote_currency_code();
                }
                None => {
                    currency_pair = CurrencyPair::from_codes(
                        symbol.quote_currency_code(),
                        commission_currency_code,
                    );

                    match self.order_book_top.get(&currency_pair) {
                        Some(top_prices) => {
                            let ask = top_prices
                                .ask
                                .as_ref()
                                .expect("There are no top ask in order book");
                            let price_quote_bnb = ask.price;
                            *converted_commission_amount = commission_amount / price_quote_bnb;
                            *converted_commission_currency_code = symbol.quote_currency_code();
                        }
                        None => log::error!(
                            "Top bids and asks for {} and currency pair {:?} do not exist",
                            self.exchange_account_id,
                            currency_pair
                        ),
                    }
                }
            }
        }
    }

    fn panic_if_fill_amounts_comformity(&self, order_filled_amount: Amount, order_ref: &OrderRef) {
        if order_filled_amount > order_ref.amount() {
            panic!(
                "filled_amount {} > order.amount {} for {} {} {:?}",
                order_filled_amount,
                order_ref.amount(),
                self.exchange_account_id,
                order_ref.client_order_id(),
                order_ref.exchange_order_id()
            )
        }
    }

    fn send_order_filled_event(
        &self,
        event_data: &FillEventData,
        order_ref: &OrderRef,
        order_fill: &OrderFill,
    ) {
        let cloned_order = Arc::new(order_ref.deep_clone());
        self.add_event_on_order_change(order_ref, OrderEventType::OrderFilled { cloned_order })
            .expect("Unable to send event, probably receiver is dropped already");

        log::info!(
            "Added a fill {} {:?} {} {:?} {:?}",
            self.exchange_account_id,
            event_data.trade_id,
            order_ref.client_order_id(),
            order_ref.exchange_order_id(),
            order_fill
        );
    }

    fn react_if_order_completed(&self, order_filled_amount: Amount, order_ref: &OrderRef) {
        if order_filled_amount == order_ref.amount() {
            order_ref.fn_mut(|order| {
                order.set_status(OrderStatus::Completed, Utc::now());
            });

            let cloned_order = Arc::new(order_ref.deep_clone());
            self.add_event_on_order_change(
                order_ref,
                OrderEventType::OrderCompleted { cloned_order },
            )
            .expect("Unable to send event, probably receiver is dropped already");
        }
    }

    fn add_fill(
        &self,
        trade_id: &Option<TradeId>,
        is_diff: bool,
        fill_type: OrderFillType,
        symbol: &Symbol,
        order_ref: &OrderRef,
        converted_commission_currency_code: CurrencyCode,
        last_fill_amount: Amount,
        last_fill_price: Price,
        last_fill_cost: Price,
        expected_commission_rate: Percent,
        commission_amount: Amount,
        order_role: OrderRole,
        commission_currency_code: CurrencyCode,
        converted_commission_amount: Amount,
    ) -> OrderFill {
        let last_fill_amount_in_converted_commission_currency_code = symbol
            .convert_amount_from_amount_currency_code(
                converted_commission_currency_code,
                last_fill_amount,
                last_fill_price,
            );
        let expected_converted_commission_amount =
            last_fill_amount_in_converted_commission_currency_code * expected_commission_rate;

        let referral_reward = self.commission.get_commission(order_role).referral_reward;
        let referral_reward_amount = commission_amount * referral_reward.percent_to_rate();

        let rounded_fill_price = symbol.price_round(last_fill_price, Round::ToNearest);

        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            Some(ClientOrderFillId::unique_id()),
            Utc::now(),
            fill_type,
            trade_id.clone(),
            rounded_fill_price,
            last_fill_amount,
            last_fill_cost,
            order_role.into(),
            commission_currency_code,
            commission_amount,
            referral_reward_amount,
            converted_commission_currency_code,
            converted_commission_amount,
            expected_converted_commission_amount,
            is_diff,
            None,
            None,
        );
        order_ref.fn_mut(|order| order.add_fill(order_fill.clone()));

        order_fill
    }

    fn create_and_add_order_fill(&self, mut event_data: &mut FillEventData, order_ref: &OrderRef) {
        let (order_fills, order_filled_amount) = order_ref.get_fills();

        if Self::was_trade_already_received(&event_data.trade_id, &order_fills, order_ref) {
            return;
        }

        if Self::diff_fill_after_non_diff(&event_data, &order_fills, order_ref) {
            return;
        }

        if Self::filled_amount_not_less_event_fill(&event_data, order_filled_amount, order_ref) {
            return;
        }

        let symbol = self
            .get_symbol(order_ref.currency_pair())
            .expect("Unable Unable to get symbol");
        let (last_fill_price, last_fill_amount, last_fill_cost) = match Self::get_last_fill_data(
            &mut event_data,
            &symbol,
            &order_fills,
            order_filled_amount,
            order_ref,
        ) {
            Some(last_fill_data) => last_fill_data,
            None => return,
        };

        if Self::should_miss_fill(
            &event_data,
            order_filled_amount,
            last_fill_amount,
            order_ref,
        ) {
            return;
        }

        Self::panic_if_wrong_status_or_cancelled(order_ref, &event_data);

        log::info!(
            "Received fill {:?} {} {}",
            event_data,
            last_fill_price,
            last_fill_amount
        );

        let commission_currency_code = event_data
            .commission_currency_code
            .unwrap_or_else(|| symbol.get_commission_currency_code(order_ref.side()));

        let order_role = Self::get_order_role(event_data, order_ref);

        let expected_commission_rate = self.set_commission_rate(&mut event_data, order_role);

        let commission_amount = Self::get_commission_amount(
            event_data.commission_amount,
            event_data.commission_rate,
            expected_commission_rate,
            last_fill_amount,
            last_fill_price,
            commission_currency_code,
            &symbol,
        );

        let mut converted_commission_currency_code = commission_currency_code;
        let mut converted_commission_amount = commission_amount;

        self.update_commission_for_bnb_case(
            commission_currency_code,
            &symbol,
            commission_amount,
            &mut converted_commission_amount,
            &mut converted_commission_currency_code,
        );

        let order_fill = self.add_fill(
            &event_data.trade_id,
            event_data.is_diff,
            event_data.fill_type,
            &symbol,
            order_ref,
            converted_commission_currency_code,
            last_fill_amount,
            last_fill_price,
            last_fill_cost,
            expected_commission_rate,
            commission_amount,
            order_role,
            commission_currency_code,
            converted_commission_amount,
        );

        // This order fields updated, so let's use actual values
        let order_filled_amount = order_ref.filled_amount();

        self.panic_if_fill_amounts_comformity(order_filled_amount, order_ref);

        self.send_order_filled_event(&event_data, order_ref, &order_fill);

        if event_data.source_type == EventSourceType::RestFallback {
            // TODO some metrics
        }

        self.react_if_order_completed(order_filled_amount, order_ref);

        // TODO DataRecorder.save(order)
    }

    fn add_external_order(&self, event_data: &mut FillEventData, args_to_log: &ArgsToLog) {
        if event_data.fill_type == OrderFillType::Liquidation
            || event_data.fill_type == OrderFillType::ClosePosition
        {
            if event_data.fill_type == OrderFillType::Liquidation
                && event_data.trade_currency_pair.is_none()
            {
                panic!(
                    "Currency pair should be set for liquidation trade {:?}",
                    &args_to_log
                );
            }

            if event_data.order_side.is_none() {
                panic!(
                    "Side should be set for liquidation or close position trade {:?}",
                    &args_to_log
                );
            }

            if event_data.client_order_id.is_some() {
                panic!(
                    "Client order id cannot be set for liquidation or close position trade {:?}",
                    &args_to_log
                );
            }

            if event_data.order_amount.is_none() {
                panic!(
                    "Order amount should be set for liquidation or close position trade {:?}",
                    &args_to_log
                );
            }

            match self
                .orders
                .cache_by_exchange_id
                .get(&event_data.exchange_order_id)
            {
                Some(order_ref) => {
                    event_data.client_order_id = Some(order_ref.client_order_id());
                }
                None => {
                    // Liquidation and ClosePosition are always Takers
                    let order_ref = self.create_order_in_pool(event_data, OrderRole::Taker);

                    event_data.client_order_id = Some(order_ref.client_order_id());
                    self.handle_create_order_succeeded(
                        self.exchange_account_id,
                        &order_ref.client_order_id(),
                        &event_data.exchange_order_id,
                        &event_data.source_type,
                    )
                    .expect("Error handle create order succeeded");
                }
            }
        }
    }

    fn create_order_in_pool(&self, event_data: &FillEventData, order_role: OrderRole) -> OrderRef {
        let currency_pair = event_data
            .trade_currency_pair
            .expect("Impossible situation: currency pair are checked above already");
        let order_amount = event_data
            .order_amount
            .clone()
            .expect("Impossible situation: amount are checked above already");
        let order_side = event_data
            .order_side
            .clone()
            .expect("Impossible situation: order_side are checked above already");

        let client_order_id = ClientOrderId::unique_id();

        let order_instance = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(order_role),
            self.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "Unknown order from handle_order_filled()",
        );

        self.orders
            .add_snapshot_initial(Arc::new(RwLock::new(order_instance)))
    }

    pub(super) fn should_ignore_event(
        allowed_event_source_type: AllowedEventSourceType,
        source_type: EventSourceType,
    ) -> bool {
        if allowed_event_source_type == AllowedEventSourceType::FallbackOnly
            && source_type != EventSourceType::RestFallback
        {
            return true;
        }

        if allowed_event_source_type == AllowedEventSourceType::NonFallback
            && source_type != EventSourceType::Rest
            && source_type != EventSourceType::WebSocket
        {
            return true;
        }

        return false;
    }
}

#[cfg(test)]
mod test {
    use anyhow::{Context, Result};
    use chrono::Utc;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::{
        exchanges::common::CurrencyCode, exchanges::general::exchange::OrderBookTop,
        exchanges::general::exchange::PriceLevel, exchanges::general::test_helper,
        exchanges::general::test_helper::create_order_ref,
        exchanges::general::test_helper::get_test_exchange, orders::fill::OrderFill,
        orders::order::OrderExecutionType, orders::order::OrderFillRole, orders::order::OrderFills,
        orders::order::OrderHeader, orders::order::OrderSimpleProps,
        orders::order::OrderStatusHistory, orders::order::SystemInternalOrderProps,
        orders::pool::OrdersPool,
    };

    fn trade_id_from_str(str: &str) -> TradeId {
        json!(str).into()
    }

    mod liquidation {
        use super::*;

        #[test]
        #[should_panic(expected = "Currency pair should be set for liquidation trad")]
        fn empty_currency_pair() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: None,
                order_side: None,
                order_amount: None,
                fill_date: None,
            };

            let (exchange, _) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);
        }

        #[test]
        #[should_panic(expected = "Side should be set for liquidation or close position trade")]
        fn empty_order_side() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
                order_side: None,
                order_amount: None,
                fill_date: None,
            };

            let (exchange, _) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);
        }

        #[test]
        #[should_panic(
            expected = "Client order id cannot be set for liquidation or close position trade"
        )]
        fn not_empty_client_order_id() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: Some(ClientOrderId::unique_id()),
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
                order_side: Some(OrderSide::Buy),
                order_amount: None,
                fill_date: None,
            };

            let (exchange, _) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);
        }

        #[test]
        #[should_panic(
            expected = "Order amount should be set for liquidation or close position trade"
        )]
        fn not_empty_order_amount() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
                order_side: Some(OrderSide::Buy),
                order_amount: None,
                fill_date: None,
            };

            let (exchange, _) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);
        }

        #[test]
        fn should_add_order() {
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let order_amount = dec!(12);
            let order_role = None;
            let fill_price = dec!(0.2);
            let fill_amount = dec!(5);

            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("test".into()),
                fill_price,
                fill_amount,
                is_diff: false,
                total_filled_amount: None,
                order_role,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(currency_pair),
                order_side: Some(order_side),
                order_amount: Some(order_amount),
                fill_date: None,
            };

            let (exchange, _event_received) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);

            let order = exchange
                .orders
                .cache_by_client_id
                .iter()
                .next()
                .expect("order should be added already");
            assert_eq!(order.order_type(), OrderType::Liquidation);
            assert_eq!(order.exchange_account_id(), exchange.exchange_account_id);
            assert_eq!(order.currency_pair(), currency_pair);
            assert_eq!(order.side(), order_side);
            assert_eq!(order.amount(), order_amount);
            assert_eq!(order.price(), fill_price);
            assert_eq!(order.role(), Some(OrderRole::Taker));

            let (fills, filled_amount) = order.get_fills();
            assert_eq!(filled_amount, fill_amount);
            assert_eq!(fills.iter().next().expect("in test").price(), fill_price);
        }

        #[test]
        #[should_panic(expected = "Received HandleOrderFilled with an empty exchangeOrderId")]
        fn empty_exchange_order_id() {
            let event_data = FillEventData {
                source_type: EventSourceType::WebSocket,
                trade_id: Some(trade_id_from_str("empty")),
                client_order_id: None,
                exchange_order_id: ExchangeOrderId::new("".into()),
                fill_price: dec!(0),
                fill_amount: dec!(0),
                is_diff: false,
                total_filled_amount: None,
                order_role: None,
                commission_currency_code: None,
                commission_rate: None,
                commission_amount: None,
                fill_type: OrderFillType::Liquidation,
                trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
                order_side: Some(OrderSide::Buy),
                order_amount: Some(dec!(0)),
                fill_date: None,
            };

            let (exchange, _event_receiver) = get_test_exchange(false);
            exchange.handle_order_filled(event_data);
        }
    }

    #[test]
    fn ignore_if_trade_was_already_received() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let order_amount = dec!(1);
        let trade_id = trade_id_from_str("test_trade_id");
        let fill_amount = dec!(0.2);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id.clone()),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            None,
            Utc::now(),
            OrderFillType::Liquidation,
            Some(trade_id),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_diff_fill_after_non_diff() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0.2);
        let order_amount = dec!(1);
        let trade_id = trade_id_from_str("test_trade_id");

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id.clone()),
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            None,
            Utc::now(),
            OrderFillType::Liquidation,
            Some(trade_id_from_str("different_trade_id")),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_filled_amount_not_less_event_fill() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("te".into(), "st".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0.2);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(CurrencyPair::from_codes("te".into(), "st".into())),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            None,
            Utc::now(),
            OrderFillType::Liquidation,
            Some(trade_id_from_str("different_trade_id")),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            false,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, fill_amount);
    }

    #[test]
    fn ignore_diff_fill_if_filled_amount_is_zero() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let order_price = dec!(1);
        let fill_amount = dec!(0);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );

        let cost = dec!(0);
        let order_fill = OrderFill::new(
            Uuid::new_v4(),
            None,
            Utc::now(),
            OrderFillType::Liquidation,
            Some(trade_id_from_str("different_trade_id")),
            order_price,
            fill_amount,
            cost,
            OrderFillRole::Taker,
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            CurrencyCode::new("test".into()),
            dec!(0),
            dec!(0),
            true,
            None,
            None,
        );
        order.add_fill(order_fill);
        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (_, order_filled_amount) = order_ref.get_fills();
        assert_eq!(order_filled_amount, dec!(0));
    }

    #[test]
    #[should_panic(expected = "Fill was received for a FailedToCreate false")]
    fn error_if_order_status_is_failed_to_create() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.set_status(OrderStatus::FailedToCreate, Utc::now());

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
    }

    #[test]
    #[should_panic(expected = "Fill was received for a Completed false")]
    fn error_if_order_status_is_completed() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.set_status(OrderStatus::Completed, Utc::now());

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
    }

    #[test]
    #[should_panic(expected = "Fill was received for a Creating true")]
    fn error_if_cancellation_event_was_raised() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(1);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let fill_price = dec!(0.2);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price,
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.internal_props.was_cancellation_event_raised = true;

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
    }

    // TODO Can be improved via testing only calculate_cost_diff_function
    #[test]
    fn calculate_cost_diff_on_buy_side() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id,
            currency_pair,
            OrderType::Limit,
            OrderSide::Buy,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            "FromTest".to_owned(),
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            header,
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        test_helper::try_add_snapshot_by_exchange_id(&exchange, &order_ref);

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(first_event_data);

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("another_trade_id")),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.3),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(second_event_data);

        let order_ref = exchange
            .orders
            .cache_by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");
        let (fills, _filled_amount) = order_ref.get_fills();

        assert_eq!(fills.len(), 2);
        let first_fill = &fills[0];
        assert_eq!(first_fill.price(), dec!(0.2));
        assert_eq!(first_fill.amount(), dec!(5));
        assert_eq!(first_fill.commission_amount(), dec!(0.01));
        let second_fill = &fills[1];
        assert_eq!(second_fill.price(), dec!(0.4));
        assert_eq!(second_fill.amount(), dec!(5));
        assert_eq!(second_fill.commission_amount(), dec!(0.02));
    }

    // TODO Can be improved via testing only calculate_cost_diff_function
    #[test]
    fn calculate_cost_diff_on_sell_side() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id,
            currency_pair,
            OrderType::Limit,
            OrderSide::Sell,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            "FromTest".to_owned(),
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            header,
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        test_helper::try_add_snapshot_by_exchange_id(&exchange, &order_ref);

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.2),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(first_event_data);

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("another_trade_id")),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.3),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(second_event_data);

        let order_ref = exchange
            .orders
            .cache_by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");
        let (fills, _filled_amount) = order_ref.get_fills();

        assert_eq!(fills.len(), 2);
        let first_fill = &fills[0];
        assert_eq!(first_fill.price(), dec!(0.2));
        assert_eq!(first_fill.amount(), dec!(5));
        assert_eq!(first_fill.commission_amount(), dec!(0.01));
        let second_fill = &fills[1];
        assert_eq!(second_fill.price(), dec!(0.4));
        assert_eq!(second_fill.amount(), dec!(5));
        assert_eq!(second_fill.commission_amount(), dec!(0.02));
    }

    #[test]
    fn calculate_cost_diff_on_buy_side_derivative() {
        let (exchange, _event_receiver) = get_test_exchange(true);

        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id,
            currency_pair,
            OrderType::Limit,
            OrderSide::Buy,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            "FromTest".to_owned(),
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            header,
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        test_helper::try_add_snapshot_by_exchange_id(&exchange, &order_ref);

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(2000),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(first_event_data);

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("another_trade_id")),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(3000),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(second_event_data);

        let order_ref = exchange
            .orders
            .cache_by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");

        let (fills, filled_amount) = order_ref.get_fills();

        assert_eq!(filled_amount, dec!(10));
        assert_eq!(fills.len(), 2);

        let first_fill = &fills[0];
        assert_eq!(first_fill.price(), dec!(2000));
        assert_eq!(first_fill.amount(), dec!(5));
        assert_eq!(first_fill.commission_amount(), dec!(0.01));

        let second_fill = &fills[1];
        assert_eq!(second_fill.price(), dec!(6000));
        assert_eq!(second_fill.amount(), dec!(5));
        assert_eq!(second_fill.commission_amount(), dec!(0.02));
    }

    // TODO Why do we need tests like this?
    // Nothing depends on order.side as I can see
    #[test]
    fn calculate_cost_diff_on_sell_side_derivative() {
        let (exchange, _event_receiver) = get_test_exchange(true);

        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id,
            currency_pair,
            OrderType::Limit,
            OrderSide::Sell,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            "FromTest".to_owned(),
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            header,
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        test_helper::try_add_snapshot_by_exchange_id(&exchange, &order_ref);

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(2000),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(first_event_data);

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("another_trade_id")),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(3000),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(second_event_data);

        let order_ref = exchange
            .orders
            .cache_by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");

        let (fills, filled_amount) = order_ref.get_fills();

        assert_eq!(filled_amount, dec!(10));
        assert_eq!(fills.len(), 2);

        let first_fill = &fills[0];
        assert_eq!(first_fill.price(), dec!(2000));
        assert_eq!(first_fill.amount(), dec!(5));
        assert_eq!(first_fill.commission_amount(), dec!(0.01));

        let second_fill = &fills[1];
        assert_eq!(second_fill.price(), dec!(6000));
        assert_eq!(second_fill.amount(), dec!(5));
        assert_eq!(second_fill.commission_amount(), dec!(0.02));
    }

    #[test]
    fn ignore_non_diff_fill_with_second_cost_lesser() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let client_order_id = ClientOrderId::unique_id();
        let order_side = OrderSide::Buy;
        let order_price = dec!(0.2);
        let order_role = OrderRole::Maker;
        let exchange_order_id: ExchangeOrderId = "some_order_id".into();

        // Add order manually for setting custom order.amount
        let header = OrderHeader::new(
            client_order_id.clone(),
            Utc::now(),
            exchange.exchange_account_id,
            currency_pair,
            OrderType::Limit,
            OrderSide::Sell,
            order_amount,
            OrderExecutionType::None,
            None,
            None,
            "FromTest".to_owned(),
        );
        let props = OrderSimpleProps::new(
            Some(order_price),
            Some(order_role),
            Some(exchange_order_id.clone()),
            Default::default(),
            Default::default(),
            Default::default(),
            None,
        );
        let order = OrderSnapshot::new(
            header,
            props,
            OrderFills::default(),
            OrderStatusHistory::default(),
            SystemInternalOrderProps::default(),
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));
        test_helper::try_add_snapshot_by_exchange_id(&exchange, &order_ref);

        let first_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.01)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(first_event_data);

        let second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("another_trade_id")),
            client_order_id: None,
            exchange_order_id: exchange_order_id.clone(),
            fill_price: dec!(0.3),
            fill_amount: dec!(10),
            is_diff: false,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(dec!(0.03)),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.handle_order_filled(second_event_data);

        let order_ref = exchange
            .orders
            .cache_by_exchange_id
            .get(&exchange_order_id)
            .expect("in test");

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);
    }

    #[test]
    fn ignore_fill_if_total_filled_amount_is_incorrect() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(1);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: Some(dec!(9)),
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert!(fills.is_empty());
    }

    #[test]
    fn take_roll_from_fill_if_specified() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: Some(OrderRole::Taker),
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            event_data.fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let fill = &fills[0];
        let right_value = dec!(0.2) / dec!(100) * dec!(5);
        assert_eq!(fill.commission_amount(), right_value);
    }

    #[test]
    fn take_roll_from_order_if_not_specified() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let fill = &fills[0];
        let right_value = dec!(0.1) / dec!(100) * dec!(5);
        assert_eq!(fill.commission_amount(), right_value);
    }

    #[test]
    #[should_panic(expected = "Fill has neither commission nor commission rate")]
    fn error_if_unable_to_get_role() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            None,
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        Exchange::get_order_role(&mut event_data, &order_ref);
    }

    #[test]
    fn use_commission_currency_code_from_event_data() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let commission_currency_code = CurrencyCode::new("BTC".into());

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: Some(commission_currency_code),
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let fill = &fills[0];
        assert_eq!(
            fill.converted_commission_currency_code(),
            commission_currency_code
        );
    }

    #[test]
    fn commission_currency_code_from_base_currency_code() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let base_currency_code = CurrencyCode::new("PHB".into());

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let fill = &fills[0];
        assert_eq!(
            fill.converted_commission_currency_code(),
            base_currency_code
        );
    }

    #[test]
    fn commission_currency_code_from_quote_currency_code() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Sell;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let quote_currency_code = exchange
            .symbols
            .iter()
            .next()
            .expect("in test")
            .value()
            .quote_currency_code;

        let fill = &fills[0];
        assert_eq!(
            fill.converted_commission_currency_code(),
            quote_currency_code
        );
    }

    #[test]
    fn use_commission_amount_if_specified() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Sell;
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let commission_amount = dec!(0.001);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: dec!(0.8),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: Some(commission_amount),
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let first_fill = &fills[0];
        assert_eq!(first_fill.commission_amount(), commission_amount);
    }

    #[test]
    fn use_commission_rate_if_specified() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Sell;
        let fill_price = dec!(0.8);
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));
        let commission_rate = dec!(0.3) / dec!(100);

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: fill_price.clone(),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: Some("BTC".into()),
            commission_rate: Some(commission_rate),
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let first_fill = &fills[0];
        let result_value = commission_rate * fill_price * fill_amount;
        assert_eq!(first_fill.commission_amount(), result_value);
    }

    #[test]
    fn calculate_commission_rate_if_not_specified() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Sell;
        let fill_price = dec!(0.8);
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: fill_price.clone(),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: Some("BTC".into()),
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let first_fill = &fills[0];
        let result_value = dec!(0.1) / dec!(100) * fill_price * fill_amount;
        assert_eq!(first_fill.commission_amount(), result_value);
    }

    #[test]
    fn calculate_commission_amount() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_price = dec!(0.8);
        let fill_amount = dec!(5);
        let order_amount = dec!(12);
        let trade_id = Some(trade_id_from_str("test_trade_id"));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id,
            client_order_id: None,
            exchange_order_id: ExchangeOrderId::new("".into()),
            fill_price: fill_price.clone(),
            fill_amount,
            is_diff: true,
            total_filled_amount: None,
            order_role: None,
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(OrderSide::Buy),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        let mut order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            dec!(0.2),
            order_amount,
            order_side,
            None,
            "FromTest",
        );
        order.fills.filled_amount = dec!(3);

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);
        let (fills, _) = order_ref.get_fills();
        assert_eq!(fills.len(), 1);

        let first_fill = &fills[0];
        let result_value = dec!(0.1) / dec!(100) * fill_amount;
        assert_eq!(first_fill.commission_amount(), result_value);
    }

    mod get_commission_amount {
        use super::*;

        #[test]
        fn from_event_data() -> Result<()> {
            let (exchange, _event_receiver) = get_test_exchange(true);

            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());

            let commission_rate = dec!(0.001);
            let expected_commission_rate = dec!(0.001);
            let last_fill_amount = dec!(5);
            let last_fill_price = dec!(0.8);
            let commission_currency_code = CurrencyCode::new("PHB".into());
            let symbol = exchange.get_symbol(currency_pair)?;
            let event_data_commission_amount = dec!(6.3);

            let commission_amount = Exchange::get_commission_amount(
                Some(event_data_commission_amount),
                Some(commission_rate),
                expected_commission_rate,
                last_fill_amount,
                last_fill_price,
                commission_currency_code,
                &symbol,
            );

            let right_value = event_data_commission_amount;
            assert_eq!(commission_amount, right_value);

            Ok(())
        }

        #[test]
        fn via_commission_rate() -> Result<()> {
            let (exchange, _event_receiver) = get_test_exchange(true);

            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());

            let commission_rate = dec!(0.001);
            let expected_commission_rate = dec!(0.001);
            let last_fill_amount = dec!(5);
            let last_fill_price = dec!(0.8);
            let commission_currency_code = CurrencyCode::new("PHB".into());
            let symbol = exchange.get_symbol(currency_pair)?;
            let commission_amount = Exchange::get_commission_amount(
                None,
                Some(commission_rate),
                expected_commission_rate,
                last_fill_amount,
                last_fill_price,
                commission_currency_code,
                &symbol,
            );

            let right_value = dec!(0.1) / dec!(100) * dec!(5) / dec!(0.8);
            assert_eq!(commission_amount, right_value);

            Ok(())
        }
    }

    mod add_fill {
        use super::*;

        #[test]
        fn expected_commission_amount_equal_commission_amount() -> Result<()> {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let order_amount = dec!(12);
            let order_role = OrderRole::Maker;
            let fill_price = dec!(0.8);

            let order_ref = create_order_ref(
                &client_order_id,
                Some(order_role),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let trade_id = Some(trade_id_from_str("test trade_id"));
            let is_diff = true;
            let symbol = exchange.get_symbol(currency_pair)?;
            let converted_commission_currency_code =
                symbol.get_commission_currency_code(order_side);
            let last_fill_amount = dec!(5);
            let last_fill_price = dec!(0.8);
            let last_fill_cost = dec!(4.0);
            let expected_commission_rate = dec!(0.001);
            let commission_currency_code = CurrencyCode::new("PHB".into());
            let converted_commission_amount = dec!(0.005);
            let commission_amount = dec!(0.1) / dec!(100) * dec!(5);

            let fill = exchange.add_fill(
                &trade_id,
                is_diff,
                OrderFillType::Liquidation,
                &symbol,
                &order_ref,
                converted_commission_currency_code,
                last_fill_amount,
                last_fill_price,
                last_fill_cost,
                expected_commission_rate,
                commission_amount,
                order_role,
                commission_currency_code,
                converted_commission_amount,
            );
            assert_eq!(fill.commission_amount(), commission_amount);
            assert_eq!(
                fill.expected_converted_commission_amount(),
                commission_amount
            );

            Ok(())
        }

        #[test]
        fn expected_commission_amount_not_equal_wrong_commission_amount() -> Result<()> {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let order_amount = dec!(12);
            let fill_price = dec!(0.8);
            let order_role = OrderRole::Maker;

            let order_ref = create_order_ref(
                &client_order_id,
                Some(order_role),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let trade_id = Some(trade_id_from_str("test trade_id"));
            let is_diff = true;
            let symbol = exchange.get_symbol(currency_pair)?;
            let converted_commission_currency_code =
                symbol.get_commission_currency_code(order_side);
            let last_fill_amount = dec!(5);
            let last_fill_price = dec!(0.8);
            let last_fill_cost = dec!(4.0);
            let expected_commission_rate = dec!(0.001);
            let commission_currency_code = CurrencyCode::new("PHB".into());
            let converted_commission_amount = dec!(0.005);
            let commission_amount = dec!(1000);

            let fill = exchange.add_fill(
                &trade_id,
                is_diff,
                OrderFillType::Liquidation,
                &symbol,
                &order_ref,
                converted_commission_currency_code,
                last_fill_amount,
                last_fill_price,
                last_fill_cost,
                expected_commission_rate,
                commission_amount,
                order_role,
                commission_currency_code,
                converted_commission_amount,
            );

            assert_eq!(fill.commission_amount(), commission_amount);
            let right_value = dec!(0.1) / dec!(100) * dec!(5);
            assert_eq!(fill.expected_converted_commission_amount(), right_value);

            Ok(())
        }

        #[test]
        fn check_referral_reward_amount() -> Result<()> {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let order_role = OrderRole::Maker;
            let order_amount = dec!(12);
            let fill_price = dec!(0.8);

            let order_ref = create_order_ref(
                &client_order_id,
                Some(order_role),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let trade_id = Some(trade_id_from_str("test trade_id"));
            let is_diff = true;
            let symbol = exchange.get_symbol(currency_pair)?;
            let converted_commission_currency_code =
                symbol.get_commission_currency_code(order_side);
            let last_fill_amount = dec!(5);
            let last_fill_price = dec!(0.8);
            let last_fill_cost = dec!(4.0);
            let expected_commission_rate = dec!(0.001);
            let commission_amount = dec!(0.005);
            let commission_currency_code = CurrencyCode::new("PHB".into());
            let converted_commission_amount = dec!(0.005);

            let fill = exchange.add_fill(
                &trade_id,
                is_diff,
                OrderFillType::Liquidation,
                &symbol,
                &order_ref,
                converted_commission_currency_code,
                last_fill_amount,
                last_fill_price,
                last_fill_cost,
                expected_commission_rate,
                commission_amount,
                order_role,
                commission_currency_code,
                converted_commission_amount,
            );

            let right_value = dec!(5) * dec!(0.1) / dec!(100) * dec!(0.4);
            assert_eq!(fill.referral_reward_amount(), right_value);

            Ok(())
        }
    }

    mod check_fill_amounts_comformity {
        use super::*;

        #[test]
        #[should_panic(expected = "filled_amount 13 > order.amount 12 fo")]
        fn too_big_filled_amount() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let fill_price = dec!(0.8);
            let order_amount = dec!(12);

            let order_ref = create_order_ref(
                &client_order_id,
                Some(OrderRole::Maker),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let fill_amount = dec!(13);
            exchange.panic_if_fill_amounts_comformity(fill_amount, &order_ref);
        }

        #[test]
        fn proper_filled_amount() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let fill_price = dec!(0.8);
            let order_amount = dec!(12);

            let order_ref = create_order_ref(
                &client_order_id,
                Some(OrderRole::Maker),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let fill_amount = dec!(10);
            exchange.panic_if_fill_amounts_comformity(fill_amount, &order_ref);
        }
    }

    mod react_if_order_completed {
        use super::*;
        use crate::exchanges::events::ExchangeEvent;

        #[test]
        fn order_completed_if_filled_completely() -> Result<()> {
            let (exchange, mut event_receiver) = get_test_exchange(false);
            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let fill_price = dec!(0.2);
            let order_amount = dec!(12);
            let order_ref = create_order_ref(
                &client_order_id,
                Some(OrderRole::Maker),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );
            let order_filled_amount = order_amount;
            exchange.react_if_order_completed(order_filled_amount, &order_ref);
            let order_status = order_ref.status();

            assert_eq!(order_status, OrderStatus::Completed);

            let event = match event_receiver
                .try_recv()
                .context("Event was not received")?
            {
                ExchangeEvent::OrderEvent(v) => v,
                _ => panic!("Should be OrderEvent"),
            };
            let gotten_id = event.order.client_order_id();
            assert_eq!(gotten_id, client_order_id);
            Ok(())
        }

        #[test]
        fn order_not_filled() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let client_order_id = ClientOrderId::unique_id();
            let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
            let order_side = OrderSide::Buy;
            let fill_price = dec!(0.2);
            let order_amount = dec!(12);

            let order_ref = create_order_ref(
                &client_order_id,
                Some(OrderRole::Maker),
                exchange.exchange_account_id,
                currency_pair,
                fill_price,
                order_amount,
                order_side,
            );

            let order_filled_amount = dec!(10);
            exchange.react_if_order_completed(order_filled_amount, &order_ref);

            let order_status = order_ref.status();

            assert_ne!(order_status, OrderStatus::Completed);
        }
    }

    mod update_commission_for_bnb_case {
        use super::*;

        #[test]
        fn using_top_bid() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let commission_currency_code = CurrencyCode::new("BNB".into());
            let symbol = exchange
                .symbols
                .iter()
                .next()
                .expect("in test")
                .value()
                .clone();
            let commission_amount = dec!(15);
            let mut converted_commission_amount = dec!(4.5);
            let mut converted_commission_currency_code = CurrencyCode::new("BTC".into());

            let currency_pair =
                CurrencyPair::from_codes(commission_currency_code, symbol.quote_currency_code);
            let order_book_top = OrderBookTop {
                ask: None,
                bid: Some(PriceLevel {
                    price: dec!(0.3),
                    amount: dec!(0.1),
                }),
            };
            exchange
                .order_book_top
                .insert(currency_pair, order_book_top);

            exchange.update_commission_for_bnb_case(
                commission_currency_code,
                &symbol,
                commission_amount,
                &mut converted_commission_amount,
                &mut converted_commission_currency_code,
            );

            let right_amount = dec!(4.5);
            assert_eq!(converted_commission_amount, right_amount);

            let right_currency_code = CurrencyCode::new("BTC".into());
            assert_eq!(converted_commission_currency_code, right_currency_code);
        }

        #[test]
        fn using_top_ask() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let commission_currency_code = CurrencyCode::new("BNB".into());
            let symbol = exchange
                .symbols
                .iter()
                .next()
                .expect("in test")
                .value()
                .clone();
            let commission_amount = dec!(15);
            let mut converted_commission_amount = dec!(4.5);
            let mut converted_commission_currency_code = CurrencyCode::new("BTC".into());

            let currency_pair = CurrencyPair::from_codes("BTC".into(), commission_currency_code);
            let order_book_top = OrderBookTop {
                ask: Some(PriceLevel {
                    price: dec!(0.3),
                    amount: dec!(0.1),
                }),
                bid: None,
            };
            exchange
                .order_book_top
                .insert(currency_pair, order_book_top);

            exchange.update_commission_for_bnb_case(
                commission_currency_code,
                &symbol,
                commission_amount,
                &mut converted_commission_amount,
                &mut converted_commission_currency_code,
            );

            let right_amount = dec!(50);
            assert_eq!(converted_commission_amount, right_amount);

            let right_currency_code = CurrencyCode::new("BTC".into());
            assert_eq!(converted_commission_currency_code, right_currency_code);
        }

        #[test]
        fn fatal_error() {
            let (exchange, _event_receiver) = get_test_exchange(false);

            let commission_currency_code = CurrencyCode::new("BNB".into());
            let symbol = exchange
                .symbols
                .iter()
                .next()
                .expect("in test")
                .value()
                .clone();
            let commission_amount = dec!(15);
            let mut converted_commission_amount = dec!(3);
            let mut converted_commission_currency_code = CurrencyCode::new("BTC".into());

            exchange.update_commission_for_bnb_case(
                commission_currency_code,
                &symbol,
                commission_amount,
                &mut converted_commission_amount,
                &mut converted_commission_currency_code,
            );

            let right_amount = dec!(3);
            assert_eq!(converted_commission_amount, right_amount);

            let right_currency_code = CurrencyCode::new("BTC".into());
            assert_eq!(converted_commission_currency_code, right_currency_code);
        }
    }

    #[test]
    fn filled_amount_from_zero_to_completed() {
        let (exchange, _event_receiver) = get_test_exchange(false);

        let client_order_id = ClientOrderId::unique_id();
        let currency_pair = CurrencyPair::from_codes("PHB".into(), "BTC".into());
        let order_side = OrderSide::Buy;
        let fill_price = dec!(0.8);
        let order_amount = dec!(12);
        let exchange_order_id = ExchangeOrderId::new("some_echange_order_id".into());
        let client_account_id = ClientOrderId::unique_id();

        let order = OrderSnapshot::with_params(
            client_order_id.clone(),
            OrderType::Liquidation,
            Some(OrderRole::Maker),
            exchange.exchange_account_id,
            currency_pair,
            fill_price,
            order_amount,
            order_side,
            None,
            "FromTest",
        );

        let order_pool = OrdersPool::new();
        let order_ref = order_pool.add_snapshot_initial(Arc::new(RwLock::new(order)));

        let mut event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("first_trade_id")),
            client_order_id: Some(client_account_id.clone()),
            exchange_order_id: exchange_order_id.clone(),
            fill_price,
            fill_amount: dec!(5),
            is_diff: true,
            total_filled_amount: None,
            order_role: Some(OrderRole::Maker),
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.create_and_add_order_fill(&mut event_data, &order_ref);

        let (_, filled_amount) = order_ref.get_fills();

        let current_right_filled_amount = dec!(5);
        assert_eq!(filled_amount, current_right_filled_amount);

        let mut second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("second_trade_id")),
            client_order_id: Some(client_account_id.clone()),
            exchange_order_id: exchange_order_id.clone(),
            fill_price,
            fill_amount: dec!(2),
            is_diff: true,
            total_filled_amount: None,
            order_role: Some(OrderRole::Maker),
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.create_and_add_order_fill(&mut second_event_data, &order_ref);

        let (_, filled_amount) = order_ref.get_fills();

        let right_filled_amount = dec!(7);
        assert_eq!(filled_amount, right_filled_amount);

        let mut second_event_data = FillEventData {
            source_type: EventSourceType::WebSocket,
            trade_id: Some(trade_id_from_str("third_trade_id")),
            client_order_id: Some(client_account_id.clone()),
            exchange_order_id: exchange_order_id.clone(),
            fill_price,
            fill_amount: dec!(5),
            is_diff: true,
            total_filled_amount: None,
            order_role: Some(OrderRole::Maker),
            commission_currency_code: None,
            commission_rate: None,
            commission_amount: None,
            fill_type: OrderFillType::Liquidation,
            trade_currency_pair: Some(currency_pair),
            order_side: Some(order_side),
            order_amount: Some(dec!(0)),
            fill_date: None,
        };

        exchange.create_and_add_order_fill(&mut second_event_data, &order_ref);

        let (_, filled_amount) = order_ref.get_fills();

        let right_filled_amount = dec!(12);
        assert_eq!(filled_amount, right_filled_amount);

        let order_status = order_ref.status();
        assert_eq!(order_status, OrderStatus::Completed);
    }
}
