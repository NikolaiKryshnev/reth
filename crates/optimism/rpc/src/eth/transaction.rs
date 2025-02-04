//! Loads and formats OP transaction RPC response.

use alloy_consensus::Signed;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types::TransactionInfo;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::Transaction;
use reth_node_api::FullNodeComponents;
use reth_primitives::{TransactionSigned, TransactionSignedEcRecovered};
use reth_provider::{BlockReaderIdExt, ReceiptProvider, TransactionsProvider};
use reth_rpc_eth_api::{
    helpers::{EthSigner, EthTransactions, LoadTransaction, SpawnBlocking},
    FromEthApiError, FullEthApiTypes, RpcNodeCore, TransactionCompat,
};
use reth_rpc_eth_types::utils::recover_raw_transaction;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};

use crate::{OpEthApi, SequencerClient};

impl<N> EthTransactions for OpEthApi<N>
where
    Self: LoadTransaction<Provider: BlockReaderIdExt>,
    N: RpcNodeCore,
{
    fn signers(&self) -> &parking_lot::RwLock<Vec<Box<dyn EthSigner>>> {
        self.inner.signers()
    }

    /// Decodes and recovers the transaction and submits it to the pool.
    ///
    /// Returns the hash of the transaction.
    async fn send_raw_transaction(&self, tx: Bytes) -> Result<B256, Self::Error> {
        let recovered = recover_raw_transaction(tx.clone())?;
        let pool_transaction =
            <Self::Pool as TransactionPool>::Transaction::from_pooled(recovered.into());

        // On optimism, transactions are forwarded directly to the sequencer to be included in
        // blocks that it builds.
        if let Some(client) = self.raw_tx_forwarder().as_ref() {
            tracing::debug!(target: "rpc::eth", hash = %pool_transaction.hash(), "forwarding raw transaction to sequencer");
            let _ = client.forward_raw_transaction(&tx).await.inspect_err(|err| {
                    tracing::debug!(target: "rpc::eth", %err, hash=% *pool_transaction.hash(), "failed to forward raw transaction");
                });
        }

        // submit the transaction to the pool with a `Local` origin
        let hash = self
            .pool()
            .add_transaction(TransactionOrigin::Local, pool_transaction)
            .await
            .map_err(Self::Error::from_eth_err)?;

        Ok(hash)
    }
}

impl<N> LoadTransaction for OpEthApi<N>
where
    Self: SpawnBlocking + FullEthApiTypes,
    N: RpcNodeCore<Provider: TransactionsProvider, Pool: TransactionPool>,
{
}

impl<N> OpEthApi<N>
where
    N: RpcNodeCore,
{
    /// Returns the [`SequencerClient`] if one is set.
    pub fn raw_tx_forwarder(&self) -> Option<SequencerClient> {
        self.sequencer_client.clone()
    }
}

impl<N> TransactionCompat for OpEthApi<N>
where
    N: FullNodeComponents,
{
    type Transaction = Transaction;

    fn fill(
        &self,
        tx: TransactionSignedEcRecovered,
        tx_info: TransactionInfo,
    ) -> Self::Transaction {
        let from = tx.signer();
        let TransactionSigned { transaction, signature, hash } = tx.into_signed();

        let inner = match transaction {
            reth_primitives::Transaction::Legacy(tx) => {
                Signed::new_unchecked(tx, signature, hash).into()
            }
            reth_primitives::Transaction::Eip2930(tx) => {
                Signed::new_unchecked(tx, signature, hash).into()
            }
            reth_primitives::Transaction::Eip1559(tx) => {
                Signed::new_unchecked(tx, signature, hash).into()
            }
            reth_primitives::Transaction::Eip4844(_) => unreachable!(),
            reth_primitives::Transaction::Eip7702(tx) => {
                Signed::new_unchecked(tx, signature, hash).into()
            }
            reth_primitives::Transaction::Deposit(tx) => OpTxEnvelope::Deposit(tx),
        };

        let deposit_receipt_version = self
            .inner
            .provider()
            .receipt_by_hash(hash)
            .ok() // todo: change sig to return result
            .flatten()
            .and_then(|receipt| receipt.deposit_receipt_version);

        let TransactionInfo { block_hash, block_number, index: transaction_index, .. } = tx_info;

        Transaction {
            inner: alloy_rpc_types::Transaction {
                inner,
                block_hash,
                block_number,
                transaction_index,
                from,
            },
            deposit_receipt_version,
        }
    }

    fn otterscan_api_truncate_input(tx: &mut Self::Transaction) {
        let input = match &mut tx.inner.inner {
            OpTxEnvelope::Eip1559(tx) => &mut tx.tx_mut().input,
            OpTxEnvelope::Eip2930(tx) => &mut tx.tx_mut().input,
            OpTxEnvelope::Legacy(tx) => &mut tx.tx_mut().input,
            OpTxEnvelope::Eip7702(tx) => &mut tx.tx_mut().input,
            OpTxEnvelope::Deposit(tx) => &mut tx.input,
            _ => return,
        };
        *input = input.slice(..4);
    }
}
