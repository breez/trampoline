use anyhow::Result;
use cln_rpc::{
    model::{
        requests::{GetinfoRequest, PayRequest},
        responses::{GetinfoResponse, PayResponse},
    },
    ClnRpc,
};

pub struct Rpc {
    rpc_file: String,
}

#[derive(Debug)]
pub enum RpcError {
    Rpc(cln_rpc::RpcError),
    General(anyhow::Error),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Rpc(rpc) => write!(f, "{}", rpc),
            RpcError::General(e) => write!(f, "{}", e),
        }
    }
}

impl From<anyhow::Error> for RpcError {
    fn from(value: anyhow::Error) -> Self {
        RpcError::General(value)
    }
}

impl From<cln_rpc::RpcError> for RpcError {
    fn from(value: cln_rpc::RpcError) -> Self {
        RpcError::Rpc(value)
    }
}

impl std::error::Error for RpcError {}

impl Rpc {
    pub fn new(rpc_file: String) -> Self {
        Self { rpc_file }
    }

    pub async fn get_info(&self) -> Result<GetinfoResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(&GetinfoRequest {}).await?)
    }

    pub async fn pay(&self, request: &PayRequest) -> Result<PayResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn rpc(&self) -> Result<ClnRpc> {
        // TODO: This creates a new unix socket connection for every payment.
        // Also, does this cause different requests to steal eachothers
        // responses in high parallelism?
        ClnRpc::new(self.rpc_file.clone()).await
    }
}
