use crate::db::model::Tables as TaikoDbTables;
use reth_db_api::{TableSet, Tables, table::TableInfo};

/// A set of tables used by Taiko network.
pub struct TaikoTables;

impl TableSet for TaikoTables {
    /// Returns an iterator over the tables, combines the default Reth tables with the
    /// Taiko-specific tables.
    fn tables() -> Box<dyn Iterator<Item = Box<dyn TableInfo>>> {
        Box::new(
            Tables::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>).chain(
                TaikoDbTables::ALL.iter().map(|table| Box::new(*table) as Box<dyn TableInfo>),
            ),
        )
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_taiko_tables_len() {
        assert!(TaikoTables::tables().count() > Tables::ALL.len());
    }
}
