//! Diagnostic: subscribe with the bot's exact filter and count updates.
use base64::Engine as _;
use futures::{sink::SinkExt, StreamExt};
use solana_sniper::proxy::{proxy_from_env, ProxyConnector};
use std::collections::HashMap;
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterTransactions,
};

#[tokio::main]
async fn main() {
    let endpoint = std::env::var("YELLOWSTONE_GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-mainnet.streaming.alchemy.com".to_string());
    let x_token = std::env::var("YELLOWSTONE_X_TOKEN").expect("YELLOWSTONE_X_TOKEN not set");
    let programs: Vec<String> = std::env::var("MONITOR_PROGRAM_IDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    println!("filter programs: {programs:?}");

    let build = GeyserGrpcClient::build_from_shared(endpoint)
        .expect("bad endpoint")
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .expect("tls")
        .x_token(Some(x_token))
        .expect("x-token");
    let mut client = build
        .connect_with_connector(ProxyConnector::from_env().expect("proxy"))
        .await
        .expect("connect");

    let mut filters = HashMap::new();
    filters.insert(
        "sniper".to_string(),
        SubscribeRequestFilterTransactions {
            vote: Some(false),
            failed: Some(false),
            account_include: programs,
            ..Default::default()
        },
    );
    let (mut sink, mut stream) = client
        .subscribe_with_request(Some(SubscribeRequest {
            transactions: filters,
            commitment: Some(CommitmentLevel::Confirmed as i32),
            ..Default::default()
        }))
        .await
        .expect("subscribe");
    println!("subscribed, holding sink, listening 60s...");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut counts: HashMap<&str, u64> = HashMap::new();
    let mut first_logs_printed = false;
    // Decode-pipeline diagnostics (mirrors the bot's process_logs).
    let mut program_log_lines: u64 = 0;
    let mut program_data_lines: u64 = 0;
    let mut b64_ok: u64 = 0;
    let mut disc_match: u64 = 0;
    let mut data_disc_match: u64 = 0;
    let mut first_disc_sample: Option<String> = None;
    // Keep the sink alive and drain any pings.
    let _sink_holder = sink;

    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next()).await;
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        let Some(msg) = msg else { break };
        let update = match msg {
            Ok(u) => u,
            Err(e) => {
                println!("stream error: {e:?}");
                break;
            }
        };
        match update.update_oneof {
            Some(UpdateOneof::Transaction(tx)) => {
                *counts.entry("transaction").or_insert(0) += 1;
                if let Some(info) = tx.transaction {
                    let logs = info.meta.map(|m| m.log_messages).unwrap_or_default();
                    if !first_logs_printed {
                        first_logs_printed = true;
                        println!("first tx slot={} sig_len={} logs={}", tx.slot, info.signature.len(), logs.len());
                        for l in logs.iter().take(12) {
                            println!("  LOG: {}", l.chars().take(160).collect::<String>());
                        }
                    }
                    // Mirror the bot's decode pipeline.
                    for l in &logs {
                        // Anchor >=0.30 emits events as "Program data: <base64>".
                        if let Some(b64) = l.strip_prefix("Program data: ") {
                            program_data_lines += 1;
                            if let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                                if payload.len() >= 8 && payload[..8] == [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee] {
                                    data_disc_match += 1;
                                }
                            }
                            continue;
                        }
                        if let Some(b64) = l.strip_prefix("Program log: ") {
                            program_log_lines += 1;
                            if let Ok(payload) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
                                b64_ok += 1;
                                if payload.len() >= 8 && payload[..8] == [0xbd, 0xdb, 0x7f, 0xd3, 0x4e, 0xe6, 0x61, 0xee] {
                                    disc_match += 1;
                                } else if first_disc_sample.is_none() && payload.len() >= 8 {
                                    first_disc_sample = Some(format!("{:02x?}", &payload[..8]));
                                }
                            }
                        }
                    }
                }
            }
            Some(UpdateOneof::Slot(s)) => {
                *counts.entry("slot").or_insert(0) += 1;
                let _ = s;
            }
            Some(_) => {
                *counts.entry("other").or_insert(0) += 1;
            }
            None => {
                *counts.entry("empty").or_insert(0) += 1;
            }
        }
    }
    println!("counts after 60s: {counts:?}");
    println!("program_log_lines={program_log_lines} b64_ok={b64_ok} trade_event_disc_match={disc_match}");
    println!("program_data_lines={program_data_lines} trade_event_in_data={data_disc_match}");
    println!("first non-matching 8-byte discriminator sample: {first_disc_sample:?}");
}
