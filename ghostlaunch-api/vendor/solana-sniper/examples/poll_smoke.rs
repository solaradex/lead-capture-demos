// Quick smoke test: connect PollingSource and pull one batch of events.
use solana_sniper::ingest::{polling::PollingSource, EventSource};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut src = PollingSource::connect().await?;
    println!("Connected OK");
    // Pull up to 3 events (each next_event does a poll if queue is empty)
    for i in 0..3 {
        match tokio::time::timeout(std::time::Duration::from_secs(60), src.next_event()).await {
            Ok(Ok(Some(ev))) => println!("Event {i}: {:?} mint={:?}", ev.kind, ev.mint),
            Ok(Ok(None)) => println!("Event {i}: None (stream ended)"),
            Ok(Err(e)) => println!("Event {i}: error: {e}"),
            Err(_) => println!("Event {i}: timeout waiting"),
        }
    }
    Ok(())
}
