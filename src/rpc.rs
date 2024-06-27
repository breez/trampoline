use anyhow::Result;
use async_trait::async_trait;
use cln_rpc::model::{
    requests::{
        DatastoreRequest, GetinfoRequest, ListdatastoreRequest, ListpaysRequest,
        ListpeerchannelsRequest, PayRequest, WaitsendpayRequest,
    },
    responses::{
        DatastoreResponse, GetinfoResponse, ListdatastoreResponse, ListpaysResponse,
        ListpeerchannelsResponse, PayResponse, WaitsendpayResponse,
    },
};
#[cfg(test)]
use mockall::automock;
#[cfg_attr(test, automock)]
#[async_trait]
pub trait ClnRpc {
    async fn datastore(&self, request: &DatastoreRequest) -> Result<DatastoreResponse, RpcError>;
    async fn get_info(&self) -> Result<GetinfoResponse, RpcError>;
    async fn listdatastore(
        &self,
        request: &ListdatastoreRequest,
    ) -> Result<ListdatastoreResponse, RpcError>;
    async fn listpays(&self, request: &ListpaysRequest) -> Result<ListpaysResponse, RpcError>;
    async fn listpeerchannels(
        &self,
        request: &ListpeerchannelsRequest,
    ) -> Result<ListpeerchannelsResponse, RpcError>;
    async fn pay(&self, request: &PayRequest) -> Result<PayResponse, RpcError>;
    async fn waitsendpay(
        &self,
        request: &WaitsendpayRequest,
    ) -> Result<WaitsendpayResponse, RpcError>;
}

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

    async fn rpc(&self) -> Result<cln_rpc::ClnRpc> {
        // TODO: This creates a new unix socket connection for every payment.
        // Also, does this cause different requests to steal eachothers
        // responses in high parallelism?
        cln_rpc::ClnRpc::new(self.rpc_file.clone()).await
    }
}

#[async_trait]
impl ClnRpc for Rpc {
    async fn datastore(&self, request: &DatastoreRequest) -> Result<DatastoreResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn get_info(&self) -> Result<GetinfoResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(&GetinfoRequest {}).await?)
    }

    async fn listdatastore(
        &self,
        request: &ListdatastoreRequest,
    ) -> Result<ListdatastoreResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn listpays(&self, request: &ListpaysRequest) -> Result<ListpaysResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn listpeerchannels(
        &self,
        request: &ListpeerchannelsRequest,
    ) -> Result<ListpeerchannelsResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn pay(&self, request: &PayRequest) -> Result<PayResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }

    async fn waitsendpay(
        &self,
        request: &WaitsendpayRequest,
    ) -> Result<WaitsendpayResponse, RpcError> {
        Ok(self.rpc().await?.call_typed(request).await?)
    }
}
