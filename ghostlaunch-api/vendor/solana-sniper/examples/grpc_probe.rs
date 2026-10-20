//! One-off probe: surface the real gRPC-layer error from Alchemy Yellowstone.
use futures::sink::SinkExt;
use solana_sniper::proxy::{proxy_from_env, ProxyConnector};
use std::collections::HashMap;
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
};

#[tokio::main]
async fn main() {
    let endpoint = std::env::var("YELLOWSTONE_GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-mainnet.streaming.alchemy.com".to_string());
    let x_token = std::env::var("YELLOWSTONE_X_TOKEN").expect("YELLOWSTONE_X_TOKEN not set");
    println!("endpoint = {endpoint}");

    // Lazy connect so the first real RPC surfaces the server's status.
    let build = GeyserGrpcClient::build_from_shared(endpoint)
        .expect("bad endpoint")
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .expect("tls")
        .x_token(Some(x_token))
        .expect("x-token");
    let mut client = if proxy_from_env().is_some() {
        println!("using CONNECT proxy");
        build
            .connect_with_connector(ProxyConnector::from_env().expect("proxy"))
            .await
            .expect("proxy connect")
    } else {
        build.connect_lazy().expect("lazy connect")
    };

    let _health = client.health_check().await;
    match _health {
        Ok(_) => println!("health_check: OK"),
        Err(e) => println!("health_check: ERR {e:?}"),
    }

    match client.subscribe().await {
        Ok((mut tx, _stream)) => {
            println!("subscribe handshake: OK, sending request...");
            let res = tx
                .send(SubscribeRequest {
                    slots: HashMap::from([("s".to_string(), Default::default())]),
                    commitment: Some(CommitmentLevel::Confirmed as i32),
                    ..Default::default()
                })
                .await;
            println!("subscribe send: {res:?}");
        }
        Err(e) => println!("subscribe: ERR {e:?}"),
    }
}
