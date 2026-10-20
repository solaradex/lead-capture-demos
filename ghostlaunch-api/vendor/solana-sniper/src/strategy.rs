use std::collections::HashSet;

use crate::types::{ChainEvent, EventKind, TradeSignal};
pub struct PumpCreatorBuyStrategy { minimum_creator_buy_sol: f64 }
impl PumpCreatorBuyStrategy {
    pub fn new(minimum_creator_buy_sol: f64) -> Self { Self { minimum_creator_buy_sol } }
    pub fn evaluate(&self, e: &ChainEvent) -> Option<TradeSignal> {
        if e.kind != EventKind::CreatorBuy { return None; }
        let mint = e.mint.as_ref()?;
        let creator = e.creator.as_ref()?;
        let amount = e.creator_buy_sol?;
        if !amount.is_finite() || amount < self.minimum_creator_buy_sol { return None; }
        Some(TradeSignal { signal_id: format!("{}:{}", e.signature, mint),
            event_id: e.event_id.clone(), mint: mint.clone(), creator: creator.clone(),
            creator_buy_sol: amount, slot: e.slot,
            virtual_sol_reserves: e.virtual_sol_reserves,
            virtual_token_reserves: e.virtual_token_reserves,
            fee_basis_points: e.fee_basis_points,
            creator_fee_basis_points: e.creator_fee_basis_points })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn qualifies_normalized_creator_buy() {
        let e: ChainEvent = serde_json::from_str(r#"{ "event_id":"e","slot":1,"signature":"s",
          "program_id":"pump","kind":"creator_buy","mint":"m","creator":"c",
          "creator_buy_sol":0.7,"observed_at_ms":10 }"#).unwrap();
        assert!(PumpCreatorBuyStrategy::new(0.5).evaluate(&e).is_some());
    }

    #[test] fn launch_snipe_fires_once_per_mint() {
        let mut s = LaunchSnipeStrategy::new();
        let mk = |sig: &str, mint: &str| ChainEvent {
            event_id: format!("{sig}:{mint}:buy"), slot: 1, signature: sig.into(),
            program_id: "pump".into(), kind: EventKind::Trade,
            mint: Some(mint.into()), creator: Some("c".into()), user: Some("u".into()),
            creator_buy_sol: Some(0.1), trade_price_sol_per_token: None, observed_at_ms: 0,
            virtual_sol_reserves: None, virtual_token_reserves: None,
            fee_basis_points: None, creator_fee_basis_points: None,
        };
        // First trade on mint1 -> signal.
        let first = s.evaluate(&mk("s1", "mint1"));
        assert!(first.is_some());
        assert_eq!(first.unwrap().mint, "mint1");
        // Second trade on mint1 -> no signal.
        assert!(s.evaluate(&mk("s2", "mint1")).is_none());
        // First trade on mint2 -> signal.
        assert!(s.evaluate(&mk("s3", "mint2")).is_some());
        // CreatorSell never triggers a launch signal.
        let mut sell = mk("s4", "mint3");
        sell.kind = EventKind::CreatorSell;
        assert!(s.evaluate(&sell).is_none());
    }

    #[test] fn raydium_migration_fires_only_for_pump_mints() {
        let mut s = RaydiumMigrationStrategy::new();
        let mk = |sig: &str, mint: &str, kind: EventKind| ChainEvent {
            event_id: format!("{sig}:{mint}:pool"), slot: 1, signature: sig.into(),
            program_id: "675kPX9MHTjS2zt1qfr1NYHuzePMKpxEqxUZ41vY9YQa".into(), kind,
            mint: Some(mint.into()), creator: None, user: None,
            creator_buy_sol: None, trade_price_sol_per_token: None, observed_at_ms: 0,
            virtual_sol_reserves: None, virtual_token_reserves: None,
            fee_basis_points: None, creator_fee_basis_points: None,
        };
        let pump_mint = "6qUw38wQ4TF8DGnpmM9jGydEQWXmgquHtmK3op5gpump";
        // Pump-migrated pool -> signal.
        let first = s.evaluate(&mk("s1", pump_mint, EventKind::RaydiumPool));
        assert!(first.is_some());
        assert!(first.unwrap().signal_id.starts_with("migration:"));
        // Same mint again -> no signal.
        assert!(s.evaluate(&mk("s2", pump_mint, EventKind::RaydiumPool)).is_none());
        // Non-pump Raydium pool -> no signal.
        assert!(s.evaluate(&mk("s3", "SomeOtherMint11111111111111111111111111", EventKind::RaydiumPool)).is_none());
        // Trade events never trigger a migration signal.
        assert!(s.evaluate(&mk("s4", pump_mint, EventKind::Trade)).is_none());
        // RaydiumPool events don't trigger the PumpSwap strategy either.
        let mut s2 = PumpSwapMigrationStrategy::new();
        assert!(s2.evaluate(&mk("s5", pump_mint, EventKind::RaydiumPool)).is_none());
    }

    #[test] fn pumpswap_migration_fires_only_for_pump_mints() {
        let mut s = PumpSwapMigrationStrategy::new();
        let mk = |sig: &str, mint: &str, kind: EventKind| ChainEvent {
            event_id: format!("{sig}:{mint}:pool"), slot: 1, signature: sig.into(),
            program_id: "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".into(), kind,
            mint: Some(mint.into()), creator: None, user: None,
            creator_buy_sol: None, trade_price_sol_per_token: None, observed_at_ms: 0,
            virtual_sol_reserves: None, virtual_token_reserves: None,
            fee_basis_points: None, creator_fee_basis_points: None,
        };
        let pump_mint = "6qUw38wQ4TF8DGnpmM9jGydEQWXmgquHtmK3op5gpump";
        // Pump-migrated pool -> signal.
        let first = s.evaluate(&mk("s1", pump_mint, EventKind::PumpSwapPool));
        assert!(first.is_some());
        assert!(first.unwrap().signal_id.starts_with("migration:"));
        // Same mint again -> no signal.
        assert!(s.evaluate(&mk("s2", pump_mint, EventKind::PumpSwapPool)).is_none());
        // Non-pump PumpSwap pool -> no signal.
        assert!(s.evaluate(&mk("s3", "SomeOtherMint11111111111111111111111111", EventKind::PumpSwapPool)).is_none());
        // Trade events never trigger a migration signal.
        assert!(s.evaluate(&mk("s4", pump_mint, EventKind::Trade)).is_none());
    }
}

/// Raydium migration snipe: fires once per mint, on the first observed
/// Raydium pool creation for a Pump.fun-migrated token (mint ends in
/// "pump"). A token only reaches Raydium when its bonding curve filled
/// (~$69k of proven buying pressure), so this signal carries validated
/// demand — unlike sniping the bare creation event. The live loop MUST run
/// [`crate::protection`] safety checks before building any buy from this signal.
#[derive(Debug, Default)]
pub struct RaydiumMigrationStrategy {
    seen_mints: HashSet<String>,
}

impl RaydiumMigrationStrategy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(&mut self, e: &ChainEvent) -> Option<TradeSignal> {
        if e.kind != EventKind::RaydiumPool {
            return None;
        }
        let mint = e.mint.as_ref()?;
        // Only Pump.fun migrations: the mint vanity suffix is "pump".
        if !mint.ends_with("pump") {
            return None;
        }
        if !self.seen_mints.insert(mint.clone()) {
            return None; // pool already seen for this mint
        }
        Some(TradeSignal {
            signal_id: format!("migration:{}:{}", e.signature, mint),
            event_id: e.event_id.clone(),
            mint: mint.clone(),
            creator: e.creator.clone().unwrap_or_default(),
            creator_buy_sol: e.creator_buy_sol.unwrap_or(0.0),
            slot: e.slot,
            virtual_sol_reserves: e.virtual_sol_reserves,
            virtual_token_reserves: e.virtual_token_reserves,
            fee_basis_points: e.fee_basis_points,
            creator_fee_basis_points: e.creator_fee_basis_points,
        })
    }
}
/// PumpSwap migration snipe: fires once per mint, on the first observed
/// PumpSwap pool creation for a Pump.fun-migrated token (mint ends in
/// "pump"). Graduated Pump.fun tokens migrate to PumpSwap — Pump.fun's own
/// AMM — not to Raydium (that was the older behavior), so this is the
/// venue the migration signal actually appears on. Same demand-validation
/// thesis as the Raydium variant: a token only reaches PumpSwap when its
/// bonding curve filled, so the signal carries validated demand — unlike
/// sniping the bare creation event. The live loop MUST run
/// [`crate::protection`] safety checks before building any buy from this signal.
#[derive(Debug, Default)]
pub struct PumpSwapMigrationStrategy {
    seen_mints: HashSet<String>,
}

impl PumpSwapMigrationStrategy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(&mut self, e: &ChainEvent) -> Option<TradeSignal> {
        if e.kind != EventKind::PumpSwapPool {
            return None;
        }
        let mint = e.mint.as_ref()?;
        // Only Pump.fun migrations: the mint vanity suffix is "pump".
        if !mint.ends_with("pump") {
            return None;
        }
        if !self.seen_mints.insert(mint.clone()) {
            return None; // pool already seen for this mint
        }
        Some(TradeSignal {
            signal_id: format!("migration:{}:{}", e.signature, mint),
            event_id: e.event_id.clone(),
            mint: mint.clone(),
            creator: e.creator.clone().unwrap_or_default(),
            creator_buy_sol: e.creator_buy_sol.unwrap_or(0.0),
            slot: e.slot,
            virtual_sol_reserves: e.virtual_sol_reserves,
            virtual_token_reserves: e.virtual_token_reserves,
            fee_basis_points: e.fee_basis_points,
            creator_fee_basis_points: e.creator_fee_basis_points,
        })
    }
}
/// Launch snipe: fires once per mint, on the first observed trade —
/// the creation moment. The live loop MUST run [`crate::protection`]
/// safety checks before building any buy from this signal.
#[derive(Debug, Default)]
pub struct LaunchSnipeStrategy {
    seen_mints: HashSet<String>,
}

impl LaunchSnipeStrategy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(&mut self, e: &ChainEvent) -> Option<TradeSignal> {
        match e.kind {
            EventKind::Trade | EventKind::CreatorBuy => {}
            _ => return None,
        }
        let mint = e.mint.as_ref()?;
        if !self.seen_mints.insert(mint.clone()) {
            return None; // not the first trade: launch already seen
        }
        Some(TradeSignal {
            signal_id: format!("launch:{}:{}", e.signature, mint),
            event_id: e.event_id.clone(),
            mint: mint.clone(),
            creator: e.creator.clone().unwrap_or_default(),
            creator_buy_sol: e.creator_buy_sol.unwrap_or(0.0),
            slot: e.slot,
            virtual_sol_reserves: e.virtual_sol_reserves,
            virtual_token_reserves: e.virtual_token_reserves,
            fee_basis_points: e.fee_basis_points,
            creator_fee_basis_points: e.creator_fee_basis_points,
        })
    }
}
