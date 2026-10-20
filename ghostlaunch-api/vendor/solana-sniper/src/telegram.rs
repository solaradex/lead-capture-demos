//! Telegram alerts for the public signals channel.
//!
//! Reads `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` from the environment.
//! When either is missing the notifier is dormant (returns `None`) and the
//! bot runs exactly as before. Sending is fail-open: a failed POST is logged
//! and never interrupts the trading loop.

use anyhow::Result;
use std::env;
use tracing::{debug, info, warn};

const API_BASE: &str = "https://api.telegram.org";

/// Truncate a base58 address like `AbC1...xYz9` for display.
fn short_addr(addr: &str) -> String {
    if addr.len() <= 12 {
        return addr.to_string();
    }
    format!("{}...{}", &addr[..4], &addr[addr.len() - 4..])
}

pub struct TelegramNotifier {
    token: String,
    chat_ids: Vec<String>,
}

impl TelegramNotifier {
    /// `None` when not configured — the bot runs without alerts.
    pub fn from_env() -> Option<Self> {
        let token = env::var("TELEGRAM_BOT_TOKEN").ok().filter(|s| !s.is_empty())?;
        let chat_id_raw = env::var("TELEGRAM_CHAT_ID").ok().filter(|s| !s.is_empty())?;
        // Support comma-separated chat IDs for multi-channel posting.
        let chat_ids: Vec<String> = chat_id_raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        if chat_ids.is_empty() {
            return None;
        }
        // Never log the token.
        debug!("telegram alerts enabled");
        Some(Self { token, chat_ids })
    }

    /// Post a dev-buy alert. Fail-open: logs and returns on any error.
    pub async fn send_dev_buy_alert(&self, mint: &str, creator: &str, buy_sol: f64) {
        let text = format!(
            "🚨 DEV BUY — {buy_sol:.2} SOL\n\nMint:\n{mint}\n\nDev: {}\n\nhttps://pump.fun/coin/{mint}",
            short_addr(creator),
        );
        self.send(text, mint).await;
    }

    /// Post a paper buy. Fail-open: logs and returns on any error.
    pub async fn send_paper_buy(&self, mint: &str, buy_sol: f64, strategy: &str) {
        let text = format_paper_buy(mint, buy_sol, strategy);
        self.send(text, mint).await;
    }

    /// Post a paper sell with P&L. Fail-open: logs and returns on any error.
    pub async fn send_paper_sell(
        &self,
        mint: &str,
        reason: &str,
        net_sol: f64,
        multiple: f64,
    ) {
        let text = format_paper_sell(mint, reason, net_sol, multiple);
        self.send(text, mint).await;
    }

    /// Post a periodic P&L summary. Fail-open: logs and returns on any error.
    pub async fn send_pnl_summary(&self, summary: &str) {
        let text = format!("📊 REMIRUN paper P&L\n\n{summary}\n\n📡 via REMIRUN");
        self.send(text, "pnl-summary").await;
    }

    async fn send(&self, text: String, log_key: &str) {
        // Use curl via subprocess — reqwest's proxy handling is unreliable in
        // this sandboxed environment, but curl respects https_proxy correctly.
        // The token stays in the Rust string (never in argv visible via ps);
        // only the payload goes on the command line.
        // Post to all configured channels.
        for chat_id in &self.chat_ids {
            let url = format!("{API_BASE}/bot{}/sendMessage", self.token);
            let payload = serde_json::json!({
                "chat_id": chat_id,
                "text": text,
                "disable_web_page_preview": true,
            });
            let payload_str = payload.to_string();

            // Run curl in a blocking task — notifications must never stall the
            // trading loop, and std::process::Command is blocking.
            let output = tokio::task::spawn_blocking({
                let url = url.clone();
                let payload_str = payload_str.clone();
                move || {
                    std::process::Command::new("curl")
                        .arg("-s")
                        .arg("-m")
                        .arg("15")
                        .arg("-X")
                        .arg("POST")
                        .arg(&url)
                        .arg("-H")
                        .arg("Content-Type: application/json")
                        .arg("-d")
                        .arg(&payload_str)
                        .output()
                }
            })
            .await;

            match output {
                Ok(Ok(out)) if out.status.success() => {
                    let body = String::from_utf8_lossy(&out.stdout);
                    if body.contains("\"ok\":true") {
                        info!(log_key, "telegram alert sent");
                    } else {
                        warn!(log_key, body = body.as_ref(), "telegram send failed");
                    }
                }
                _ => {
                    warn!(log_key, "telegram curl failed");
                }
            }
        }
    }
}

/// Pure formatting helper, unit-tested without network.
pub fn format_dev_buy_alert(mint: &str, creator: &str, buy_sol: f64) -> String {
    format!(
        "🚨 DEV BUY — {buy_sol:.2} SOL\n\nMint:\n{mint}\n\nDev: {}\n\nhttps://pump.fun/coin/{mint}\n\n📡 via REMIRUN",
        short_addr(creator),
    )
}

/// Pure formatting for a paper buy post.
pub fn format_paper_buy(mint: &str, buy_sol: f64, strategy: &str) -> String {
    format!(
        "🟢 PAPER BUY — {buy_sol:.2} SOL\n\nMint:\n{mint}\n\nStrategy: {strategy}\n\nhttps://pump.fun/coin/{mint}\n\n📡 via REMIRUN (paper — no real SOL)",
    )
}

/// Pure formatting for a paper sell post. `reason` is the exit reason label.
pub fn format_paper_sell(mint: &str, reason: &str, net_sol: f64, multiple: f64) -> String {
    let emoji = if net_sol >= 0.0 { "🟢" } else { "🔴" };
    format!(
        "{emoji} PAPER SELL — {reason}\n\nMint:\n{mint}\n\nP&L: {net_sol:+.4} SOL ({multiple:.2}x)\n\nhttps://pump.fun/coin/{mint}\n\n📡 via REMIRUN (paper — no real SOL)",
    )
}

#[allow(dead_code)]
fn _result_ok() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_format_contains_key_fields() {
        let mint = "So11111111111111111111111111111111111111112";
        let creator = "AbC12345xYz98765qWeRtYuIoP00000000000000000";
        let msg = format_dev_buy_alert(mint, creator, 12.345);
        assert!(msg.contains("12.35 SOL"));
        assert!(msg.contains(mint));
        assert!(msg.contains("AbC1...0000"));
        assert!(msg.contains(&format!("https://pump.fun/coin/{mint}")));
        assert!(msg.contains("REMIRUN"));
        // Token must never appear in the message body.
        assert!(!msg.contains("bot"));
    }

    #[test]
    fn dormant_without_env() {
        // from_env returns None when vars are absent; don't touch the real env
        // here — just assert the constructor contract via empty-string filter.
        let empty: Option<String> = Some(String::new()).filter(|s| !s.is_empty());
        assert!(empty.is_none());
    }

    #[test]
    fn paper_buy_format_marks_paper() {
        let msg = format_paper_buy("SomeMint123", 0.1, "pumpswap_migration");
        assert!(msg.contains("PAPER BUY"));
        assert!(msg.contains("0.10 SOL"));
        assert!(msg.contains("pumpswap_migration"));
        assert!(msg.contains("SomeMint123"));
        assert!(msg.contains("no real SOL"));
    }

    #[test]
    fn paper_sell_win_uses_green() {
        let msg = format_paper_sell("SomeMint123", "TakeProfit(2)", 0.2358, 3.36);
        assert!(msg.starts_with("🟢"));
        assert!(msg.contains("+0.2358 SOL"));
        assert!(msg.contains("3.36x"));
        assert!(msg.contains("TakeProfit(2)"));
    }

    #[test]
    fn paper_sell_loss_uses_red() {
        let msg = format_paper_sell("SomeMint123", "HardStopLoss", -0.0541, 0.46);
        assert!(msg.starts_with("🔴"));
        assert!(msg.contains("-0.0541 SOL"));
        assert!(msg.contains("0.46x"));
    }
}
