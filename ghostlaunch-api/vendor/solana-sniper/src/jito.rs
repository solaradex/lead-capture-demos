//! Jito block-engine relay: bundle submission + landing verification.
//!
//! `JitoRelay::new` takes the block-engine **base** URL, e.g.
//! `https://ny.mainnet.block-engine.jito.wtf` (region closest to you).
//! Submission acks are NOT fills: always verify landing with
//! [`JitoRelay::wait_for_landing`] before registering a position.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::execution::{ExecutionRelay, Submission};

/// Sends caller-provided, already-signed base64 transactions. Does not sign/build or verify fills.
pub struct JitoRelay {
    base_url: String,
    client: Client,
}

impl JitoRelay {
    /// `base_url` like `https://ny.mainnet.block-engine.jito.wtf`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: Client::new(),
        }
    }

    /// `sendBundle` is served at /api/v1/bundles (the method rides in the
    /// JSON-RPC envelope). /api/v1/transactions is the single-tx proxy.
    fn send_bundle_url(&self) -> String {
        format!("{}/api/v1/bundles", self.base_url)
    }

    fn bundle_status_url(&self) -> String {
        format!("{}/api/v1/bundles", self.base_url)
    }

    /// Query `getBundleStatuses` once. Returns `Ok(None)` when the bundle is
    /// unknown (not yet seen by the block engine).
    pub async fn get_bundle_status(
        &self,
        bundle_id: &str,
    ) -> Result<Option<BundleStatus>> {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id]],
        });
        let resp: serde_json::Value = self
            .client
            .post(&self.bundle_status_url())
            .json(&body)
            .send()
            .await
            .context("getBundleStatuses request")?
            .error_for_status()
            .context("getBundleStatuses http error")?
            .json()
            .await
            .context("parsing getBundleStatuses response")?;
        if let Some(e) = resp.get("error") {
            return Err(anyhow!("Jito RPC error: {e}"));
        }
        match resp.pointer("/result/value/0") {
            None => Ok(None),
            Some(v) if v.is_null() => Ok(None),
            Some(v) => {
                // Field naming differs across Jito docs/versions; accept both.
                let confirmation_status = v
                    .get("confirmation_status")
                    .or_else(|| v.get("confirmationStatus"))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                let slot = v.get("slot").and_then(|s| s.as_u64());
                let err = v.get("err").and_then(|e| {
                    if e.is_null() {
                        None
                    } else {
                        Some(e.to_string())
                    }
                });
                Ok(Some(BundleStatus {
                    bundle_id: bundle_id.to_string(),
                    slot,
                    confirmation_status,
                    err,
                }))
            }
        }
    }

    /// Poll until the bundle lands (`confirmed`/`finalized`), errors, or
    /// `attempts` run out. Returns the last observed status on timeout.
    pub async fn wait_for_landing(
        &self,
        bundle_id: &str,
        attempts: usize,
        interval: Duration,
    ) -> Result<BundleStatus> {
        let mut last: Option<BundleStatus> = None;
        for i in 0..attempts {
            if let Some(st) = self.get_bundle_status(bundle_id).await? {
                let landed = matches!(
                    st.confirmation_status.as_deref(),
                    Some("finalized") | Some("confirmed")
                );
                if landed || st.err.is_some() {
                    return Ok(st);
                }
                last = Some(st);
            }
            if i + 1 < attempts {
                sleep(interval).await;
            }
        }
        last.ok_or_else(|| {
            anyhow!("bundle {bundle_id} has no status after {attempts} checks")
        })
    }
}

/// Observed bundle status from `getBundleStatuses`.
#[derive(Debug, Clone)]
pub struct BundleStatus {
    pub bundle_id: String,
    pub slot: Option<u64>,
    /// `processed` | `confirmed` | `finalized` (when known).
    pub confirmation_status: Option<String>,
    /// Non-null when the bundle errored.
    pub err: Option<String>,
}

impl BundleStatus {
    pub fn landed(&self) -> bool {
        self.err.is_none()
            && matches!(
                self.confirmation_status.as_deref(),
                Some("finalized") | Some("confirmed")
            )
    }
}

/// JSON-RPC envelope for Jito `sendBundle`.
/// Jito's documented params shape is:
///   params: [ [tx1_base64, tx2_base64, ...], {"encoding": "base64"} ]
/// i.e. params[0] is a FLAT array of transaction strings. (A previous
/// version wrapped the tx array in an extra `[..; 1]`, which serialized
/// params[0] as [[txs...]] and made the block engine return 400
/// Bad Request on every submission — both endpoints, Oct 16 2026.)
#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: (&'a [String], Encoding),
}
#[derive(Serialize)]
struct Encoding {
    encoding: &'static str,
}
#[derive(Deserialize)]
struct RpcResponse {
    result: Option<String>,
    error: Option<serde_json::Value>,
}
#[async_trait]
impl ExecutionRelay for JitoRelay {
    async fn submit_signed_transactions(&self, txs: &[String]) -> Result<Submission> {
        if txs.is_empty() || txs.len() > 5 {
            return Err(anyhow!("bundle must contain 1 to 5 transactions"));
        }
        if txs.iter().any(|t| t.trim().is_empty()) {
            return Err(anyhow!("empty transaction payload"));
        }
        let body = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendBundle",
            // params[0] must be the flat tx array, not wrapped in another array
            params: (txs, Encoding {
                encoding: "base64",
            }),
        };
        let res: RpcResponse = self
            .client
            .post(&self.send_bundle_url())
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("parsing Jito response")?;
        if let Some(e) = res.error {
            return Err(anyhow!("Jito RPC error: {e}"));
        }
        Ok(Submission {
            bundle_id: res
                .result
                .ok_or_else(|| anyhow!("missing bundle ID"))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_built_from_base() {
        let r = JitoRelay::new("https://ny.mainnet.block-engine.jito.wtf/");
        assert_eq!(
            r.send_bundle_url(),
            "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles"
        );
        assert_eq!(
            r.bundle_status_url(),
            "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles"
        );
    }

    #[test]
    fn landing_predicate() {
        let ok = BundleStatus {
            bundle_id: "b".into(),
            slot: Some(1),
            confirmation_status: Some("finalized".into()),
            err: None,
        };
        assert!(ok.landed());
        let pending = BundleStatus {
            confirmation_status: Some("processed".into()),
            ..ok.clone()
        };
        assert!(!pending.landed());
        let failed = BundleStatus {
            confirmation_status: Some("finalized".into()),
            err: Some(r#"{"InstructionError":1}"#.into()),
            ..ok.clone()
        };
        assert!(!failed.landed());
    }

    #[test]
    fn send_bundle_body_matches_jito_api_format() {
        // Regression test for the Oct 16 2026 400 Bad Request bug: the
        // params array must be [[tx1, tx2], {"encoding": "base64"}] with
        // params[0] a FLAT array of transaction strings. An extra nesting
        // level makes the block engine reject every submission.
        let txs = vec!["AAA".to_string(), "BBB".to_string()];
        let body = RpcRequest {
            jsonrpc: "2.0",
            id: 1,
            method: "sendBundle",
            params: (
                &txs[..],
                Encoding {
                    encoding: "base64",
                },
            ),
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["method"], "sendBundle");
        assert_eq!(v["params"][1]["encoding"], "base64");
        // params[0] is the flat tx array — NOT wrapped in another array.
        assert_eq!(
            v["params"][0],
            serde_json::json!(["AAA", "BBB"]),
            "params[0] must be a flat array of tx strings per Jito's sendBundle docs"
        );
    }
}
