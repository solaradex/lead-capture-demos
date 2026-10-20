use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ChainEvent {
    pub event_id: String, pub slot: u64, pub signature: String, pub program_id: String,
    pub kind: EventKind, pub mint: Option<String>, pub creator: Option<String>,
    #[serde(default)] pub user: Option<String>,
    #[serde(default)] pub creator_buy_sol: Option<f64>,
    #[serde(default)] pub trade_price_sol_per_token: Option<f64>,
    /// Virtual SOL reserves right after the trade (lamports), when the
    /// underlying event carried them. Used to quote `buy_exact_sol_in`.
    #[serde(default)] pub virtual_sol_reserves: Option<u64>,
    /// Virtual token reserves right after the trade (base units).
    #[serde(default)] pub virtual_token_reserves: Option<u64>,
    /// Protocol fee charged on the trade, basis points.
    #[serde(default)] pub fee_basis_points: Option<u64>,
    /// Creator fee charged on the trade, basis points.
    #[serde(default)] pub creator_fee_basis_points: Option<u64>,
    pub observed_at_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind { Launch, CreatorBuy, CreatorSell, Trade, RaydiumPool, PumpSwapPool, Other }
#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub signal_id: String, pub event_id: String, pub mint: String, pub creator: String,
    pub creator_buy_sol: f64, pub slot: u64,
    /// Curve state at signal time for quoting `buy_exact_sol_in`.
    /// None when the underlying event didn't carry it.
    pub virtual_sol_reserves: Option<u64>,
    pub virtual_token_reserves: Option<u64>,
    pub fee_basis_points: Option<u64>,
    pub creator_fee_basis_points: Option<u64>,
}
