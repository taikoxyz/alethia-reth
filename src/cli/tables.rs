use crate::db::model::Tables as TaikoDbTables;
use reth_db::{TableSet, Tables, table::TableInfo};

pub struct TaikoTables;

impl TableSet for TaikoTables {
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(
            Tables::ALL
                .iter()
                .map(|table| Box::new(*table) as Box<dyn TableInfo>)
                .chain(
                    TaikoDbTables::ALL
                        .iter()
                        .map(|table| Box::new(*table) as Box<dyn TableInfo>),
                ),
        )
    }
}
