use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::orders::order::OrderSnapshot;
use crate::orders::pool::OrderRef;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OrderEventType {
    CreateOrderSucceeded,
    CreateOrderFailed,
    OrderFilled { cloned_order: Arc<OrderSnapshot> },
    OrderCompleted { cloned_order: Arc<OrderSnapshot> },
    CancelOrderSucceeded,
    CancelOrderFailed,
}

#[derive(Debug, Clone)]
pub struct OrderEvent {
    pub order: OrderRef,
    pub event_type: OrderEventType,
}

impl OrderEvent {
    pub fn new(order: OrderRef, event_type: OrderEventType) -> Self {
        Self { order, event_type }
    }
}
