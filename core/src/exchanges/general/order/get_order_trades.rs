use crate::exchanges::common::{Amount, CurrencyCode, ExchangeError, Price};
use crate::exchanges::events::TradeId;
use crate::exchanges::general::exchange::RequestResult;
use crate::exchanges::general::helpers::{get_rest_error, handle_parse_error};
use crate::exchanges::general::symbol::Symbol;
use crate::orders::fill::OrderFillType;
use crate::orders::order::{ExchangeOrderId, OrderRole};
use crate::{
    exchanges::general::{exchange::Exchange, features::RestFillsType},
    orders::pool::OrderRef,
};
use anyhow::{bail, Context, Result};
use itertools::Itertools;
use mmb_utils::DateTime;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct OrderTrade {
    pub exchange_order_id: ExchangeOrderId,
    pub trade_id: TradeId,
    pub datetime: DateTime,
    pub price: Price,
    pub amount: Amount,
    pub order_role: OrderRole,
    pub fee_currency_code: CurrencyCode,
    pub fee_rate: Option<Price>,
    pub fee_amount: Option<Amount>,
    pub fill_type: OrderFillType,
}

impl OrderTrade {
    pub fn new(
        exchange_order_id: ExchangeOrderId,
        trade_id: TradeId,
        datetime: DateTime,
        price: Price,
        amount: Amount,
        order_role: OrderRole,
        fee_currency_code: CurrencyCode,
        fee_rate: Option<Price>,
        fee_amount: Option<Amount>,
        fill_type: OrderFillType,
    ) -> Self {
        Self {
            exchange_order_id,
            trade_id,
            datetime,
            price,
            amount,
            order_role,
            fee_currency_code,
            fee_rate,
            fee_amount,
            fill_type,
        }
    }
}

impl Exchange {
    pub async fn get_order_trades(
        &self,
        symbol: &Symbol,
        order: &OrderRef,
    ) -> Result<RequestResult<Vec<OrderTrade>>> {
        let fills_type = &self.features.rest_fills_features.fills_type;
        match fills_type {
            RestFillsType::MyTrades => self.get_my_trades_with_filter(symbol, order).await,
            _ => bail!("Fills type {:?} is not supported", fills_type),
        }
    }

    async fn get_my_trades_with_filter(
        &self,
        symbol: &Symbol,
        order: &OrderRef,
    ) -> Result<RequestResult<Vec<OrderTrade>>> {
        let my_trades = self.get_my_trades(symbol, None).await?;
        match my_trades {
            RequestResult::Error(_) => Ok(my_trades),
            RequestResult::Success(my_trades) => {
                let exchange_order_id = order
                    .exchange_order_id()
                    .with_context(|| format!("There is no exchange_order in order {:?}", order))?;

                let data = my_trades
                    .into_iter()
                    .filter(|order_trade| order_trade.exchange_order_id == exchange_order_id)
                    .collect_vec();

                Ok(RequestResult::Success(data))
            }
        }
    }

    pub async fn get_my_trades(
        &self,
        symbol: &Symbol,
        last_date_time: Option<DateTime>,
    ) -> Result<RequestResult<Vec<OrderTrade>>> {
        // TODO Add metric UseTimeMetric(RequestType::GetMyTrades)
        let response = self
            .exchange_client
            .request_my_trades(symbol, last_date_time)
            .await?;

        match get_rest_error(
            &response,
            self.exchange_account_id,
            self.features.empty_response_is_ok,
        ) {
            Some(error) => Ok(RequestResult::Error(error)),
            None => match self
                .exchange_client
                .parse_get_my_trades(&response, last_date_time)
            {
                Ok(data) => Ok(RequestResult::Success(data)),
                Err(error) => {
                    handle_parse_error(
                        error,
                        &response,
                        "".into(),
                        None,
                        self.exchange_account_id,
                    )?;
                    Ok(RequestResult::Error(ExchangeError::unknown_error(
                        &response.content,
                    )))
                }
            },
        }
    }
}
