pub mod receipt;

/// A ledger keeping settled and pending amounts independently.
pub struct Ledger {
    settled: i64,
    pending: i64,
    label: String,
}
impl Ledger {
    pub fn new(label: String) -> Self { Self { settled: 0, pending: 0, label } }
    pub fn deposit(&mut self, amount: i64) { self.pending += normalize(amount); }
    pub fn settle(&mut self) { self.settled += self.pending; self.pending = 0; }
    pub fn balance(&self) -> i64 { self.settled + self.pending }
    pub fn label(&self) -> &str { &self.label }
}
impl Ledger {
    pub fn receipt(&self) -> String { format!("{}: {}", self.label(), self.balance()) }
}
pub fn normalize(amount: i64) -> i64 { amount.max(0) }
pub fn preview(amount: i64) -> i64 { normalize(amount) + 1 }
pub fn explain(flag: bool) -> bool { if flag { true } else { false } }
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn normalized_deposit() { let mut ledger = Ledger::new("test".into()); ledger.deposit(4); assert_eq!(ledger.balance(), 4); }
}
