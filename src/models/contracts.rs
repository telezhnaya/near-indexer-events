use bigdecimal::BigDecimal;
use sqlx::Arguments;

use crate::models::FieldCount;

#[derive(Debug, Clone, sqlx::FromRow, FieldCount)]
pub struct Contract {
    pub contract_account_id: String,
    pub standard: String,
    pub first_event_at_timestamp: BigDecimal,
    pub first_event_at_block_height: BigDecimal,
    pub inconsistency_found_at_timestamp: Option<BigDecimal>,
    pub inconsistency_found_at_block_height: Option<BigDecimal>,
    // ignored by the db
    pub should_add_to_db: bool,
}

impl crate::models::SqlMethods for Contract {
    fn add_to_args(&self, args: &mut sqlx::postgres::PgArguments) {
        args.add(&self.contract_account_id);
        args.add(&self.standard);
        args.add(&self.first_event_at_timestamp);
        args.add(&self.first_event_at_block_height);
        args.add(&self.inconsistency_found_at_timestamp);
        args.add(&self.inconsistency_found_at_block_height);
    }

    fn insert_query(items_count: usize) -> anyhow::Result<String> {
        Ok("INSERT INTO contracts VALUES ".to_owned()
            // -1 because of the flag should_add_to_db
            + &crate::models::create_placeholders(items_count, Contract::field_count() - 1)?
            + " ON CONFLICT (contract_account_id) DO UPDATE SET "
            + " inconsistency_found_at_timestamp = excluded.inconsistency_found_at_timestamp, "
            + " inconsistency_found_at_block_height = excluded.inconsistency_found_at_block_height")
    }

    fn name() -> String {
        "contracts".to_string()
    }
}
