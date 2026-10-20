use anyhow::Result;
use async_trait::async_trait;
#[derive(Debug, Clone)]
pub struct Submission { pub bundle_id: String }
#[async_trait]
pub trait ExecutionRelay: Send + Sync {
    async fn submit_signed_transactions(&self, base64_transactions: &[String]) -> Result<Submission>;
}
pub struct DisabledRelay;
#[async_trait]
impl ExecutionRelay for DisabledRelay {
    async fn submit_signed_transactions(&self, _: &[String]) -> Result<Submission> {
        anyhow::bail!("execution is disabled in this scaffold")
    }
}
