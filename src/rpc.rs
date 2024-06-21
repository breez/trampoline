use anyhow::Result;
use cln_rpc::{
    model::{requests::GetinfoRequest, responses::GetinfoResponse},
    ClnRpc,
};

pub struct Rpc {
    rpc_file: String,
}

impl Rpc {
    pub fn new(rpc_file: String) -> Self {
        Self { rpc_file }
    }

    pub async fn get_info(&self) -> Result<GetinfoResponse> {
        Ok(self.rpc().await?.call_typed(&GetinfoRequest {}).await?)
    }

    async fn rpc(&self) -> Result<ClnRpc> {
        ClnRpc::new(self.rpc_file.clone()).await
    }
}
