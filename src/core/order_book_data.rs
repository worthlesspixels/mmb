use rust_decimal::*;
use std::collections::HashMap;

type OrderDataMap = HashMap<Decimal, Decimal>;

pub struct OrderBookData {
    pub asks: OrderDataMap,
    pub bids: OrderDataMap,
}

impl OrderBookData {
    // TODO Здесь потенциально должно быть несколько конструкторов. А как?
    // Разными именами методами
    pub fn new(asks: OrderDataMap, bids: OrderDataMap) -> Self {
        Self { asks, bids }
    }

    // Сделать просто Vec вторым параметром
    pub fn update(&mut self, updates: Vec<OrderBookData>) {
        // If exists at least one update
        if updates.is_empty() {
            return;
        }

        self.update_inner_data(updates);
    }

    fn update_inner_data(&mut self, updates: Vec<OrderBookData>) {
        for update in updates.iter() {
            for (key, amount) in update.bids.iter() {
                self.bids.insert(*key, *amount);
            }

            for (key, amount) in update.asks.iter() {
                self.asks.insert(*key, *amount);
            }

            // TODO remove elements where value == 0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_asks() {
        // Prepare data for updates
        let mut first_update_asks = OrderDataMap::new();
        first_update_asks.insert(Decimal::new(1), Decimal::new(2));
        first_update_asks.insert(Price(3), Amount(4));

        let mut first_update_bids = OrderDataMap::new();
        first_update_bids.insert(Price(1), Amount(2));
        first_update_bids.insert(Price(3), Amount(4));

        // TODO Full tests with several updates
        //let mut second_update_asks = OrderDataMap::new();
        //second_update_asks.insert(5, 6);
        //second_update_asks.insert(7, 8);

        //let mut second_update_bids = OrderDataMap::new();
        //second_update_bids.insert(5, 6);
        //second_update_bids.insert(7, 8);

        // Create updates
        let first_update = OrderBookData::new(first_update_asks, first_update_bids);
        //let second_update = OrderBookData::new(second_update_asks, second_update_bids);

        let updates = vec![first_update];
        //updates.push_back(second_update);

        // Prepare updated object
        let mut primary_asks = OrderDataMap::new();
        primary_asks.insert(Price(1), Amount(1));
        primary_asks.insert(Price(3), Amount(1));

        let mut primary_bids = OrderDataMap::new();
        primary_asks.insert(Price(1), Amount(1));
        primary_asks.insert(Price(3), Amount(1));
        let mut main_order_data = OrderBookData::new(primary_asks, primary_bids);

        main_order_data.update(updates);

        assert_eq!(main_order_data.asks.get(&Price(3)), Some(&Amount(4)));
    }

    #[test]
    fn empty_update() {}
}
