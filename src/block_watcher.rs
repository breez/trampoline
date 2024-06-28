use anyhow::Result;
use async_trait::async_trait;
use std::{sync::Arc, time::Duration};
use tracing::{debug, error, instrument, trace};

use tokio::{
    sync::{mpsc::Receiver, Mutex},
    task::JoinHandle,
};

use crate::{
    messages::BlockAdded,
    rpc::{ClnRpc, Rpc},
};
#[cfg(test)]
use mockall::automock;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// A block provider provides the current chain tip.
#[cfg_attr(test, automock)]
#[async_trait]
pub trait BlockProvider {
    /// Gets the current best height of the chain.
    async fn current_height(&self) -> u32;
}

/// The BlockWatcher periodically polls core lightning for the latest block and
/// subscribes to block_added events in order to always contain the latest block
/// height.
pub struct BlockWatcher {
    /// Core lightning rpc client
    rpc: Arc<Rpc>,

    /// The current best height of the chain.
    current_height: Arc<Mutex<u32>>,
}

/// BlockWatcher implementation
impl BlockWatcher {
    /// Initializes a new BlockWatcher.
    pub fn new(rpc: Arc<Rpc>) -> Self {
        Self {
            rpc,
            current_height: Arc::new(Mutex::new(0)),
        }
    }

    /// Starts watching for the current height in the background until the
    /// shutdown receiver is invoked. Polls core lightning for the latest block
    /// height every 60 seconds.
    #[instrument(skip_all)]
    pub async fn start(&mut self, shutdown: Receiver<()>) -> Result<JoinHandle<()>> {
        poll_height(Arc::clone(&self.current_height), Arc::clone(&self.rpc)).await?;
        let current_height = Arc::clone(&self.current_height);
        let rpc = Arc::clone(&self.rpc);
        Ok(tokio::spawn(poll_forever(shutdown, current_height, rpc)))
    }

    /// Hook for the block_added event. Updates the height based on this event.
    #[instrument(skip(self))]
    pub async fn new_block(&self, block: &BlockAdded) {
        match update_height(block.height, Arc::clone(&self.current_height)).await {
            Ok(new_height) => debug!(blockheight = new_height, "Blockheight updated"),
            Err(e) => error!("Failed to update blockheight: {:?}", e),
        };
    }
}

#[async_trait]
impl BlockProvider for BlockWatcher {
    /// Gets the current best height of the chain.
    async fn current_height(&self) -> u32 {
        *self.current_height.lock().await
    }
}

/// Polls core lightning for the latest blockheight until the shutdown receiver
/// is invoked.
#[instrument(skip_all)]
async fn poll_forever(mut shutdown: Receiver<()>, current_height: Arc<Mutex<u32>>, rpc: Arc<Rpc>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {},
            _ = shutdown.recv() => return
        }

        match poll_height(Arc::clone(&current_height), Arc::clone(&rpc)).await {
            Ok(height) => match height {
                Some(height) => debug!(blockheight = height, "blockheight updated"),
                None => trace!("no blockheight update"),
            },
            // TODO: Panic?
            Err(e) => error!("Failed to update block height: {:?}", e),
        }
    }
}

/// Polls the current blockheight from core lightning a single time.
async fn poll_height(current_height: Arc<Mutex<u32>>, rpc: Arc<Rpc>) -> Result<Option<u32>> {
    let info = rpc.get_info().await?;
    update_height(info.blockheight, current_height).await
}

/// Updates the current height if the new height is better.
async fn update_height(new_height: u32, current_height: Arc<Mutex<u32>>) -> Result<Option<u32>> {
    let mut current_height = current_height.lock().await;
    let updated = if new_height > *current_height {
        *current_height = new_height;
        Some(*current_height)
    } else {
        None
    };

    Ok(updated)
}
