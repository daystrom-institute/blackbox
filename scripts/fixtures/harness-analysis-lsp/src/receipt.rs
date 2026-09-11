use crate::{Ledger, normalize};
pub fn total(amount: i64) -> i64 { normalize(amount) }
pub fn render(ledger: &Ledger) -> String { ledger.receipt() }
