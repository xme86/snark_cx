// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    helpers::{assign_to_worker, init_worker_channels, Batch, PrimaryReceiver, PrimarySender},
    BatchPropose,
    BatchSealed,
    BatchSignature,
    Event,
    Gateway,
    Shared,
    Worker,
    MAX_WORKERS,
};
use snarkos_account::Account;
use snarkos_node_messages::Data;
use snarkvm::{
    console::prelude::*,
    prelude::{
        block::Transaction,
        coinbase::{ProverSolution, PuzzleCommitment},
        Signature,
    },
};

use parking_lot::{Mutex, RwLock};
use std::{collections::HashMap, future::Future, net::SocketAddr, sync::Arc};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct Primary<N: Network> {
    /// The shared state.
    shared: Arc<Shared<N>>,
    /// The gateway.
    gateway: Gateway<N>,
    /// The workers.
    workers: Arc<RwLock<Vec<Worker<N>>>>,
    /// The currently-proposed batch, along with its signatures.
    proposed_batch: Arc<RwLock<Option<(Batch<N>, Vec<Signature<N>>)>>>,
    /// The spawned handles.
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl<N: Network> Primary<N> {
    /// Initializes a new primary instance.
    pub fn new(shared: Arc<Shared<N>>, account: Account<N>, dev: Option<u16>) -> Result<Self> {
        // Construct the gateway instance.
        let gateway = Gateway::new(shared.clone(), account, dev)?;
        // Return the primary instance.
        Ok(Self {
            shared,
            gateway,
            workers: Default::default(),
            proposed_batch: Default::default(),
            handles: Default::default(),
        })
    }

    /// Returns the gateway.
    pub const fn gateway(&self) -> &Gateway<N> {
        &self.gateway
    }

    /// Returns the number of workers.
    pub fn num_workers(&self) -> u8 {
        u8::try_from(self.workers.read().len()).expect("Too many workers")
    }

    /// Run the primary instance.
    pub async fn run(&mut self, sender: PrimarySender<N>, receiver: PrimaryReceiver<N>) -> Result<()> {
        info!("Starting the primary instance of the memory pool...");

        // Set the primary sender.
        self.shared.set_primary_sender(sender);

        // Construct a map of the worker senders.
        let mut tx_workers = HashMap::new();

        // Initialize the workers.
        for _ in 0..MAX_WORKERS {
            // Construct the worker ID.
            let id = u8::try_from(self.workers.read().len())?;
            // Construct the worker channels.
            let (tx_worker, rx_worker) = init_worker_channels();
            // Construct the worker instance.
            let mut worker = Worker::new(id, self.gateway.clone())?;
            // Run the worker instance.
            worker.run(rx_worker).await?;
            // Add the worker to the list of workers.
            self.workers.write().push(worker);
            // Add the worker sender to the map.
            tx_workers.insert(id, tx_worker);
        }

        // Initialize the gateway.
        self.gateway.run(tx_workers).await?;

        // Start the primary handlers.
        self.start_handlers(receiver);

        Ok(())
    }

    /// Proposes the batch for the current round.
    ///
    /// This method performs the following steps:
    /// 1. Drain the workers.
    /// 2. Sign the batch.
    /// 3. Set the batch in the primary.
    /// 4. Broadcast the batch to all validators for signing.
    pub fn propose_batch(&self) -> Result<()> {
        // Initialize the RNG.
        let mut rng = rand::thread_rng();

        // Initialize a map of the transmissions.
        let mut transmissions = HashMap::new();
        // Drain the workers.
        for worker in self.workers.read().iter() {
            // TODO (howardwu): Perform one final filter against the ledger service.
            // Transition the worker to the next round, and add their transmissions to the map.
            transmissions.extend(worker.drain());
        }

        // Retrieve the current round.
        let round = self.shared.round();
        // Retrieve the previous certificates.
        let previous_certificates = self.shared.previous_certificates(round).unwrap_or_default();
        // Sign the batch.
        let batch =
            Batch::new(self.gateway.account().private_key(), round, transmissions, previous_certificates, &mut rng)?;

        // Set the proposed batch.
        *self.proposed_batch.write() = Some((batch.clone(), vec![]));

        // Broadcast the batch to all validators for signing.
        self.gateway.broadcast(Event::BatchPropose(BatchPropose::new(Data::Object(batch))));
        Ok(())
    }
}

impl<N: Network> Primary<N> {
    /// Starts the primary handlers.
    fn start_handlers(&self, receiver: PrimaryReceiver<N>) {
        let PrimaryReceiver {
            mut rx_batch_propose,
            mut rx_batch_signature,
            mut rx_batch_sealed,
            mut rx_unconfirmed_solution,
            mut rx_unconfirmed_transaction,
        } = receiver;

        // Start the batch proposer.
        self.start_batch_proposer();
        // Start the batch sealer.
        self.start_batch_sealer();

        // Process the proposed batch.
        let self_clone = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, batch_propose)) = rx_batch_propose.recv().await {
                if let Err(e) = self_clone.process_batch_propose_from_peer(peer_ip, batch_propose).await {
                    error!("Failed to process a batch propose from peer '{peer_ip}': {e}");
                }
            }
        });

        // Process the batch signature.
        let self_clone = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, batch_signature)) = rx_batch_signature.recv().await {
                if let Err(e) = self_clone.process_batch_signature_from_peer(peer_ip, batch_signature).await {
                    error!("Failed to process a batch signature from peer '{peer_ip}': {e}");
                }
            }
        });

        // Process the sealed batch.
        let self_clone = self.clone();
        self.spawn(async move {
            while let Some((peer_ip, batch_certificate)) = rx_batch_sealed.recv().await {
                // Deserialize the batch certificate.
                let Ok(batch_certificate) = batch_certificate.deserialize().await else {
                    error!("Failed to deserialize the batch certificate from peer '{peer_ip}'");
                    continue;
                };
                // Store the sealed batch in the shared state.
                self_clone.shared.store_sealed_batch(peer_ip, batch_certificate);
            }
        });

        // Process the unconfirmed solutions.
        let self_clone = self.clone();
        self.spawn(async move {
            while let Some((puzzle_commitment, prover_solution)) = rx_unconfirmed_solution.recv().await {
                // Compute the worker ID.
                let Ok(worker_id) = assign_to_worker(puzzle_commitment, self_clone.num_workers()) else {
                    error!("Unable to determine the worker ID for the unconfirmed solution");
                    continue;
                };
                // Retrieve the worker.
                let worker = self_clone.workers.read()[worker_id as usize].clone();
                // Process the unconfirmed solution.
                if let Err(e) = worker.process_unconfirmed_solution(puzzle_commitment, prover_solution).await {
                    error!("Worker {} failed process a message: {e}", worker.id());
                }
            }
        });

        // Process the unconfirmed transactions.
        let self_clone = self.clone();
        self.spawn(async move {
            while let Some((transaction_id, transaction)) = rx_unconfirmed_transaction.recv().await {
                // Compute the worker ID.
                let Ok(worker_id) = assign_to_worker::<N>(&transaction_id, self_clone.num_workers()) else {
                    error!("Unable to determine the worker ID for the unconfirmed transaction");
                    continue;
                };
                // Retrieve the worker.
                let worker = self_clone.workers.read()[worker_id as usize].clone();
                // Process the unconfirmed transaction.
                if let Err(e) = worker.process_unconfirmed_transaction(transaction_id, transaction).await {
                    error!("Worker {} failed process a message: {e}", worker.id());
                }
            }
        });
    }

    /// Starts the batch proposer.
    fn start_batch_proposer(&self) {
        // Initialize the batch proposer.
        let self_clone = self.clone();
        self.spawn(async move {
            // TODO: Implement proper timeouts to propose a batch. Need to sync the primaries.
            // Sleep.
            tokio::time::sleep(std::time::Duration::from_millis(5000)).await;
            loop {
                // If there is a proposed batch, wait for it to be sealed.
                if self_clone.proposed_batch.read().is_some() {
                    // Sleep briefly, but longer than if there were no batch.
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    continue;
                }

                // If there is no proposed batch, propose one.
                if let Err(e) = self_clone.propose_batch() {
                    error!("Failed to propose a batch: {e}");
                }
            }
        });
    }

    /// Starts the batch sealer.
    fn start_batch_sealer(&self) {
        // Initialize the batch sealer.
        let self_clone = self.clone();
        self.spawn(async move {
            loop {
                // Initialize flags to track operations to perform after reading.
                let mut is_expired = false;
                let mut is_ready = false;

                // If there is no batch, wait for one to be proposed.
                if self_clone.proposed_batch.read().is_none() {
                    // Sleep briefly, but longer than if there were a batch.
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                    continue;
                }

                // If there is a batch, check if it is expired or ready to be sealed.
                if let Some((batch, signatures)) = self_clone.proposed_batch.read().clone() {
                    // TODO (howardwu): Use stake checks.
                    // // If the batch is expired, clear it.
                    // is_expired = batch.timestamp() + BATCH_EXPIRATION
                    //     < SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                    // // If the batch is ready to be sealed, seal it.
                    // is_ready = signatures.len() >= self_clone.shared.num_validators();
                    if !signatures.is_empty() {
                        is_ready = true;
                    }
                }

                // If the batch is expired, clear it.
                if is_expired {
                    *self_clone.proposed_batch.write() = None;
                }
                // If the batch is ready to be sealed, seal it.
                if is_ready {
                    // Retrieve the batch and signatures, clearing the proposed batch.
                    let (batch, signatures) = self_clone.proposed_batch.write().take().unwrap();
                    // Seal the batch.
                    let sealed_batch = batch.seal(signatures);
                    // Fetch the certificate.
                    let certificate = sealed_batch.certificate().clone();
                    // Fetch the address.
                    let address = self_clone.gateway.account().address();
                    // Store the sealed batch in the shared state.
                    self_clone.shared.store_sealed_batch_from_primary(address, sealed_batch);

                    // Create a batch sealed event.
                    let event = BatchSealed::new(Data::Object(certificate));
                    // Broadcast the sealed batch to all validators.
                    self_clone.gateway.broadcast(Event::BatchSealed(event));
                    // TODO: Increment the round.
                    info!("\n\n\n\nA batch has been sealed!\n\n\n");
                }

                // Sleep briefly.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
    }

    /// Processes a batch propose from a peer.
    async fn process_batch_propose_from_peer(&self, peer_ip: SocketAddr, batch_propose: BatchPropose<N>) -> Result<()> {
        // // Retrieve the current round.
        // let round = self.shared.round();
        // Deserialize the batch.
        let batch = batch_propose.batch.deserialize().await?;

        // TODO (howardwu): Verify the batch.

        // Store the proposed batch in the shared state.
        self.shared.store_proposed_batch(peer_ip, batch.clone());

        // Initialize an RNG.
        let rng = &mut rand::thread_rng();
        // Sign the batch ID.
        let signature = self.gateway.account().sign(&[batch.batch_id()], rng)?;
        // Broadcast the signature back to the validator.
        self.gateway.send(peer_ip, Event::BatchSignature(BatchSignature::new(batch.batch_id(), signature)));
        Ok(())
    }

    /// Processes a batch signature from a peer.
    async fn process_batch_signature_from_peer(
        &self,
        peer_ip: SocketAddr,
        batch_signature: BatchSignature<N>,
    ) -> Result<()> {
        // Retrieve the batch ID and signature.
        let BatchSignature { batch_id, signature } = batch_signature;

        // Ensure the batch ID matches the currently proposed batch.
        if Some(batch_id) != self.proposed_batch.read().as_ref().map(|(batch, _)| batch.batch_id()) {
            warn!("Received a batch signature for an unknown batch ID '{batch_id}' from peer '{peer_ip}'");
            return Ok(());
        }
        // Retrieve the address of the peer.
        let Some(address) = self.shared.get_address(&peer_ip) else {
            warn!("Received a batch signature from a disconnected peer '{peer_ip}'");
            return Ok(());
        };
        // Ensure the address is in the committee.
        if !self.shared.is_committee_member(&address) {
            warn!("Received a batch signature from a non-committee peer '{peer_ip}'");
            return Ok(());
        }
        // Verify the signature.
        if !signature.verify(&address, &[batch_id]) {
            warn!("Received an invalid batch signature from peer '{peer_ip}'");
            return Ok(());
        }

        // Add the signature to the batch.
        if let Some((_, signatures)) = self.proposed_batch.write().as_mut() {
            info!("Added a batch signature from peer '{peer_ip}'");
            signatures.push(signature);
        }
        Ok(())
    }

    /// Spawns a task with the given future; it should only be used for long-running tasks.
    fn spawn<T: Future<Output = ()> + Send + 'static>(&self, future: T) {
        self.handles.lock().push(tokio::spawn(future));
    }

    /// Shuts down the primary.
    pub async fn shut_down(&self) {
        trace!("Shutting down the primary...");
        // Iterate through the workers.
        self.workers.read().iter().for_each(|worker| {
            // Shut down the worker.
            worker.shut_down();
        });
        // Abort the tasks.
        self.handles.lock().iter().for_each(|handle| handle.abort());
        // Close the gateway.
        self.gateway.shut_down().await;
    }
}
