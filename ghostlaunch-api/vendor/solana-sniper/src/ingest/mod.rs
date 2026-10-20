use anyhow::{Context, Result};
use async_trait::async_trait;
use std::{fs::File, io::{BufRead, BufReader, Lines}, path::Path};
use crate::types::ChainEvent;

pub mod polling;
pub mod yellowstone;

#[async_trait]
pub trait EventSource { async fn next_event(&mut self) -> Result<Option<ChainEvent>>; }
pub struct ReplaySource { lines: Lines<BufReader<File>> }
impl ReplaySource {
    pub fn from_path(p: impl AsRef<Path>) -> Result<Self> {
        let file = File::open(p.as_ref()).with_context(|| format!("opening {}", p.as_ref().display()))?;
        Ok(Self { lines: BufReader::new(file).lines() })
    }
}
#[async_trait]
impl EventSource for ReplaySource {
    async fn next_event(&mut self) -> Result<Option<ChainEvent>> {
        loop {
            match self.lines.next() {
                None => return Ok(None),
                Some(Err(e)) => return Err(e.into()),
                Some(Ok(line)) if line.trim().is_empty() => continue,
                Some(Ok(line)) => return Ok(Some(serde_json::from_str(&line)
                    .with_context(|| format!("invalid replay JSON: {line}"))?)),
            }
        }
    }
}
// TODO: implement a real Yellowstone source using a pinned, verified client/protobuf API.
