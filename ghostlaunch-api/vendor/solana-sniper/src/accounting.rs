use std::collections::HashSet;
#[derive(Default)]
pub struct SeenEvents { ids: HashSet<String> }
impl SeenEvents {
    /// In-process only. Production requires durable idempotency across restarts.
    pub fn insert(&mut self, id: String) -> bool { self.ids.insert(id) }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionStatus { Open, ExitPending, RecoveryRequired, Closed }
#[derive(Debug, Clone)]
pub struct PositionRecord {
    pub mint: String, pub entry_signature: String, pub initial_token_amount: u64,
    pub remaining_token_amount: u64, pub status: PositionStatus,
}
// TODO: persist positions and reconcile actual balances from chain before registering fills.
