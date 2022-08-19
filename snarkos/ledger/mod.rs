// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

mod server;
pub use server::*;

use crate::{handle_dispatch_error, BlockDB, ProgramDB};
use snarkvm::prelude::*;

use colored::Colorize;
use futures::StreamExt;
use indexmap::IndexMap;
use once_cell::race::OnceBox;
use parking_lot::RwLock;
use std::{net::SocketAddr, sync::Arc};
use tokio::task;
pub(crate) type InternalLedger<N> = snarkvm::prelude::Ledger<N, BlockDB<N>, ProgramDB<N>>;
// pub(crate) type InternalLedger<N> = snarkvm::prelude::Ledger<N, BlockMemory<N>, ProgramMemory<N>>;

#[allow(dead_code)]
pub struct Ledger<N: Network> {
    /// The internal ledger.
    ledger: RwLock<InternalLedger<N>>,
    /// The current peers.
    peers: RwLock<IndexMap<SocketAddr, crate::Sender<N>>>,
    /// The server.
    server: OnceBox<Server<N>>,
    /// The account private key.
    private_key: PrivateKey<N>,
    /// The account view key.
    view_key: ViewKey<N>,
    /// The account address.
    address: Address<N>,
}

impl<N: Network> Ledger<N> {
    /// Initializes a new instance of the ledger.
    pub async fn load(private_key: &PrivateKey<N>) -> Result<Arc<Self>> {
        // Derive the view key and address.
        let view_key = ViewKey::try_from(private_key)?;
        let address = Address::try_from(&view_key)?;

        // Initialize the ledger.
        let ledger = Arc::new(Self {
            ledger: RwLock::new(InternalLedger::open()?),
            peers: RwLock::new(IndexMap::new()),
            server: OnceBox::new(),
            private_key: *private_key,
            view_key,
            address,
        });

        // Initialize the server.
        let server = Server::<N>::start(ledger.clone())?;
        ledger
            .server
            .set(Box::new(server))
            .map_err(|_| anyhow!("Failed to save the server"))?;

        // Sync the ledger with the network.
        ledger.initial_sync_with_network().await?;
        // Return the ledger.
        Ok(ledger)
    }

    /// Returns the ledger.
    pub(super) const fn ledger(&self) -> &RwLock<InternalLedger<N>> {
        &self.ledger
    }

    /// Returns the connected peers.
    pub(super) const fn peers(&self) -> &RwLock<IndexMap<SocketAddr, crate::Sender<N>>> {
        &self.peers
    }
}

impl<N: Network> Ledger<N> {
    /// Adds the given transaction to the memory pool.
    pub fn add_to_memory_pool(&self, transaction: Transaction<N>) -> Result<()> {
        self.ledger.write().add_to_memory_pool(transaction)
    }

    /// Advances the ledger to the next block.
    pub async fn advance_to_next_block(self: &Arc<Self>) -> Result<Block<N>> {
        let self_clone = self.clone();
        let next_block = task::spawn_blocking(move || {
            // Initialize an RNG.
            let rng = &mut ::rand::thread_rng();
            // Propose the next block.
            self_clone.ledger.read().propose_next_block(&self_clone.private_key, rng)
        })
        .await??;

        // Ensure the given block is a valid next block.
        self.ledger.read().check_next_block(&next_block)?;

        // Add the next block to the ledger.
        let self_clone = self.clone();
        let next_block_clone = next_block.clone();
        if let Err(error) = task::spawn_blocking(move || self_clone.ledger.write().add_next_block(&next_block_clone)).await? {
            // Log the error.
            warn!("{error}");
        }

        // Broadcast the block to all peers.
        let peers = self.peers().read().clone();
        for (_, sender) in peers.iter() {
            let _ = sender.send(crate::Message::<N>::BlockBroadcast(next_block.clone())).await;
        }

        // Return the next block.
        Ok(next_block)
    }
}

// Internal operations.
impl<N: Network> Ledger<N> {
    /// Returns the unspent records.
    pub fn find_unspent_records(&self) -> Result<IndexMap<Field<N>, Record<N, Plaintext<N>>>> {
        // Fetch the unspent records.
        let records = self
            .ledger
            .read()
            .find_records(&self.view_key, RecordsFilter::Unspent)?
            .filter(|(_, record)| !record.gates().is_zero())
            .collect::<IndexMap<_, _>>();
        // Return the unspent records.
        Ok(records)
    }

    /// Returns the spent records.
    pub fn find_spent_records(&self) -> Result<IndexMap<Field<N>, Record<N, Plaintext<N>>>> {
        // Fetch the unspent records.
        let records = self
            .ledger
            .read()
            .find_records(&self.view_key, RecordsFilter::Spent)?
            .filter(|(_, record)| !record.gates().is_zero())
            .collect::<IndexMap<_, _>>();
        // Return the unspent records.
        Ok(records)
    }

    /// Creates a deploy transaction.
    pub fn create_deploy(&self, program: &Program<N>, additional_fee: u64) -> Result<Transaction<N>> {
        // Fetch the unspent records.
        let records = self.find_unspent_records()?;
        ensure!(!records.len().is_zero(), "The Aleo account has no records to spend.");

        // Prepare the additional fee.
        let credits = records.values().max_by(|a, b| (**a.gates()).cmp(&**b.gates())).unwrap().clone();
        ensure!(
            ***credits.gates() >= additional_fee,
            "The additional fee is more than the record balance."
        );

        // Initialize an RNG.
        let rng = &mut ::rand::thread_rng();
        // Deploy.
        let transaction = Transaction::deploy(self.ledger.read().vm(), &self.private_key, program, (credits, additional_fee), rng)?;
        // Verify.
        assert!(self.ledger.read().vm().verify(&transaction));
        // Return the transaction.
        Ok(transaction)
    }

    /// Creates a transfer transaction.
    pub fn create_transfer(
        &self,
        to_address: Address<N>,
        vk: Option<ViewKey<N>>,
        pk: Option<PrivateKey<N>>,
        amount: u64,
    ) -> Result<Transaction<N>> {
        let vk = vk.unwrap_or(self.view_key);
        let pk = pk.unwrap_or(self.private_key);
        // Fetch the unspent record with the least gates.
        let record = self
            .ledger
            .read()
            .find_records(&vk, RecordsFilter::Unspent)?
            .filter(|(_, record)| !record.gates().is_zero())
            .min_by(|(_, a), (_, b)| (**a.gates()).cmp(&**b.gates()));

        // Prepare the record.
        let record = match record {
            Some((_, record)) => record,
            None => bail!("The Aleo account has no records to spend."),
        };

        // Create a new transaction.
        Transaction::execute(
            self.ledger.read().vm(),
            &pk,
            &ProgramID::from_str("credits.aleo")?,
            Identifier::from_str("transfer")?,
            &[
                Value::Record(record),
                Value::from_str(&format!("{to_address}"))?,
                Value::from_str(&format!("{amount}u64"))?,
            ],
            None,
            &mut ::rand::thread_rng(),
        )
    }
}

// Internal operations.
impl<N: Network> Ledger<N> {
    /// Syncs the ledger with the network.
    async fn initial_sync_with_network(self: &Arc<Self>) -> Result<()> {
        /// The number of concurrent requests with the network.
        const CONCURRENT_REQUESTS: usize = 100;
        /// Url to fetch the blocks from.
        const TARGET_URL: &str = "https://vm.aleo.org/testnet3/block/testnet3/";
        // TODO (raychu86): Move this declaration out.
        /// The IP of the leader node.
        const LEADER_IP: &str = "http://159.203.77.113";

        // Fetch the ledger height.
        let ledger_height = self.ledger.read().latest_height();

        // Fetch the latest height.
        let latest_height = reqwest::get(format!("{LEADER_IP}/testnet3/latest/height"))
            .await?
            .text()
            .await?
            .parse::<u32>()?;

        // Start a timer.
        let timer = std::time::Instant::now();

        // Sync the ledger to the latest block height.
        if latest_height > ledger_height + 1 {
            futures::stream::iter((ledger_height + 1)..=latest_height)
                .map(|height| {
                    trace!("Requesting block {height} of {latest_height}");

                    // Download the block with an exponential backoff retry policy.
                    handle_dispatch_error(move || async move {
                        // Get the URL for the block download.
                        let block_url = format!("{TARGET_URL}{height}.block");

                        // Fetch the bytes from the given url
                        reqwest::get(block_url).await
                    })
                })
                .buffered(CONCURRENT_REQUESTS)
                .for_each(|response| async {
                    // Use blocking tasks, as deserialization and adding blocks are expensive operations.
                    let self_clone = self.clone();
                    let block_bytes = response.unwrap().bytes().await;

                    task::spawn_blocking(move || {
                        // Parse the block.
                        let block = Block::from_bytes_le(&block_bytes.unwrap()).unwrap();

                        // Add the block to the ledger.
                        self_clone.ledger.write().add_next_block(&block).unwrap();

                        // Retrieve the current height.
                        let height = block.height();
                        // Compute the percentage completed.
                        let percentage = height * 100 / latest_height;
                        // Compute the time remaining (in seconds).
                        let time_remaining = (timer.elapsed().as_secs() + 1) * (latest_height - height) as u64 / height as u64;
                        // Prepare the estimate message.
                        let estimate = format!("(est. {} minutes remaining)", time_remaining / 60u64);
                        // Log the progress.
                        info!(
                            "Synced up to block {height} of {latest_height} - {percentage}% complete {}",
                            estimate.dimmed()
                        );
                    })
                    .await
                    .unwrap();
                })
                .await;
        }

        Ok(())
    }
}
