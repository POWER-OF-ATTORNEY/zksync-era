use std::sync::Arc;

use anyhow::Context as _;
use zksync_concurrency::{ctx, error::Wrap as _, scope, time};
use zksync_consensus_executor::{self as executor, attestation};
use zksync_consensus_roles::{attester, validator};
use zksync_consensus_storage::{BlockStore, PersistentBlockStore as _};
use zksync_dal::consensus_dal;
use zksync_node_sync::{fetcher::FetchedBlock, sync_action::ActionQueueSender, SyncState};
use zksync_types::L2BlockNumber;
use zksync_web3_decl::{
    client::{DynClient, L2},
    error::is_retryable,
    jsonrpsee::{core::ClientError, types::error::ErrorCode},
    namespaces::{EnNamespaceClient as _, EthNamespaceClient as _},
};

use super::{config, storage::Store, ConsensusConfig, ConsensusSecrets};
use crate::{
    metrics::METRICS,
    registry,
    storage::{self, ConnectionPool},
};

/// Whenever more than FALLBACK_FETCHER_THRESHOLD certificates are missing,
/// the fallback fetcher is active.
pub(crate) const FALLBACK_FETCHER_THRESHOLD: u64 = 10;

/// External node.
pub(super) struct EN {
    pub(super) pool: ConnectionPool,
    pub(super) sync_state: SyncState,
    pub(super) client: Box<DynClient<L2>>,
}

impl EN {
    /// Task running a consensus node for the external node.
    /// It may be a validator, but it cannot be a leader (cannot propose blocks).
    pub async fn run(
        self,
        ctx: &ctx::Ctx,
        actions: ActionQueueSender,
        cfg: ConsensusConfig,
        secrets: ConsensusSecrets,
        build_version: Option<semver::Version>,
    ) -> anyhow::Result<()> {
        let attester = config::attester_key(&secrets).context("attester_key")?;

        tracing::debug!(
            is_attester = attester.is_some(),
            "external node attester mode"
        );

        let res: ctx::Result<()> = scope::run!(ctx, |ctx, s| async {
            // Update sync state in the background.
            s.spawn_bg(self.fetch_state_loop(ctx));

            // Initialize global config.
            let global_config = self
                .fetch_global_config(ctx)
                .await
                .wrap("fetch_global_config()")?;
            let mut conn = self.pool.connection(ctx).await.wrap("connection()")?;

            conn.try_update_global_config(ctx, &global_config)
                .await
                .wrap("try_update_global_config()")?;

            let payload_queue = conn
                .new_payload_queue(ctx, actions, self.sync_state.clone())
                .await
                .wrap("new_payload_queue()")?;

            drop(conn);

            // Monitor the genesis of the main node.
            // If it changes, it means that a hard fork occurred and we need to reset the consensus state.
            s.spawn_bg::<()>({
                let old = global_config.clone();
                async {
                    let old = old;
                    loop {
                        if let Ok(new) = self.fetch_global_config(ctx).await {
                            // We verify the transition here to work around the situation
                            // where `consensus_global_config()` RPC fails randomly and fallback
                            // to `consensus_genesis()` RPC activates.
                            if new != old
                                && consensus_dal::verify_config_transition(&old, &new).is_ok()
                            {
                                return Err(anyhow::format_err!(
                                    "global config changed: old {old:?}, new {new:?}"
                                )
                                .into());
                            }
                        }
                        ctx.sleep(time::Duration::seconds(5)).await?;
                    }
                }
            });

            // Run consensus component.
            // External nodes have a payload queue which they use to fetch data from the main node.
            let (store, runner) = Store::new(
                ctx,
                self.pool.clone(),
                Some(payload_queue),
                Some(self.client.clone()),
            )
            .await
            .wrap("Store::new()")?;
            s.spawn_bg(async { Ok(runner.run(ctx).await.context("Store::runner()")?) });

            // Run the temporary fetcher until the certificates are backfilled.
            // Temporary fetcher should be removed once json RPC syncing is fully deprecated.
            s.spawn_bg({
                let store = store.clone();
                async {
                    let store = store;
                    self.fallback_block_fetcher(ctx, &store)
                        .await
                        .wrap("fallback_block_fetcher()")
                }
            });

            let (block_store, runner) = BlockStore::new(ctx, Box::new(store.clone()))
                .await
                .wrap("BlockStore::new()")?;
            s.spawn_bg(async { Ok(runner.run(ctx).await.context("BlockStore::run()")?) });

            let attestation = Arc::new(attestation::Controller::new(attester));
            s.spawn_bg({
                let global_config = global_config.clone();
                let attestation = attestation.clone();
                async {
                    let res = self
                        .run_attestation_controller(ctx, global_config, attestation)
                        .await
                        .wrap("run_attestation_controller()");
                    // Attestation currently is not critical for the node to function.
                    // If it fails, we just log the error and continue.
                    if let Err(err) = res {
                        tracing::error!("attestation controller failed: {err:#}");
                    }
                    Ok(())
                }
            });

            let executor = executor::Executor {
                config: config::executor(&cfg, &secrets, &global_config, build_version)?,
                block_store,
                validator: config::validator_key(&secrets)
                    .context("validator_key")?
                    .map(|key| executor::Validator {
                        key,
                        replica_store: Box::new(store.clone()),
                        payload_manager: Box::new(store.clone()),
                    }),
                attestation,
            };
            tracing::info!("running the external node executor");
            executor.run(ctx).await?;

            Ok(())
        })
        .await;
        match res {
            Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }

    /// Task fetching L2 blocks using JSON-RPC endpoint of the main node.
    pub async fn run_fetcher(
        self,
        ctx: &ctx::Ctx,
        actions: ActionQueueSender,
    ) -> anyhow::Result<()> {
        tracing::warn!("\
            WARNING: this node is using ZKsync API synchronization, which will be deprecated soon. \
            Please follow this instruction to switch to p2p synchronization: \
            https://github.com/matter-labs/zksync-era/blob/main/docs/src/guides/external-node/10_decentralization.md");
        let res: ctx::Result<()> = scope::run!(ctx, |ctx, s| async {
            // Update sync state in the background.
            s.spawn_bg(self.fetch_state_loop(ctx));
            let mut payload_queue = self
                .pool
                .connection(ctx)
                .await
                .wrap("connection()")?
                .new_payload_queue(ctx, actions, self.sync_state.clone())
                .await
                .wrap("new_fetcher_cursor()")?;
            self.fetch_blocks(ctx, &mut payload_queue).await
        })
        .await;
        match res {
            Ok(()) | Err(ctx::Error::Canceled(_)) => Ok(()),
            Err(ctx::Error::Internal(err)) => Err(err),
        }
    }

    /// Monitors the `AttestationStatus` on the main node,
    /// and updates the attestation config accordingly.
    async fn run_attestation_controller(
        &self,
        ctx: &ctx::Ctx,
        cfg: consensus_dal::GlobalConfig,
        attestation: Arc<attestation::Controller>,
    ) -> ctx::Result<()> {
        const POLL_INTERVAL: time::Duration = time::Duration::seconds(5);
        let registry = registry::Registry::new(self.pool.clone()).await;
        let mut next = attester::BatchNumber(0);
        loop {
            let status = loop {
                match self
                    .fetch_attestation_status(ctx)
                    .await
                    .wrap("fetch_attestation_status()")
                {
                    Err(err) => tracing::warn!("{err:#}"),
                    Ok(status) => {
                        if status.genesis != cfg.genesis.hash() {
                            return Err(anyhow::format_err!("genesis mismatch").into());
                        }
                        if status.next_batch_to_attest >= next {
                            break status;
                        }
                    }
                }
                ctx.sleep(POLL_INTERVAL).await?;
            };
            next = status.next_batch_to_attest.next();
            tracing::info!(
                "waiting for hash of batch {:?}",
                status.next_batch_to_attest
            );
            let hash = consensus_dal::batch_hash(
                &self
                    .pool
                    .wait_for_batch_info(ctx, status.next_batch_to_attest, POLL_INTERVAL)
                    .await
                    .wrap("wait_for_batch_info()")?,
            );
            let Some(committee) = registry
                .attester_committee_for(
                    ctx,
                    cfg.registry_address.map(registry::Address::new),
                    status.next_batch_to_attest,
                )
                .await
                .wrap("attester_committee_for()")?
            else {
                tracing::info!("attestation not required");
                continue;
            };
            let committee = Arc::new(committee);
            // Persist the derived committee.
            self.pool
                .connection(ctx)
                .await
                .wrap("connection")?
                .upsert_attester_committee(ctx, status.next_batch_to_attest, &committee)
                .await
                .wrap("upsert_attester_committee()")?;
            tracing::info!(
                "attesting batch {:?} with hash {hash:?}",
                status.next_batch_to_attest
            );
            attestation
                .start_attestation(Arc::new(attestation::Info {
                    batch_to_attest: attester::Batch {
                        genesis: status.genesis,
                        hash,
                        number: status.next_batch_to_attest,
                    },
                    committee: committee.clone(),
                }))
                .await
                .context("start_attestation()")?;
        }
    }

    /// Periodically fetches the head of the main node
    /// and updates `SyncState` accordingly.
    async fn fetch_state_loop(&self, ctx: &ctx::Ctx) -> ctx::Result<()> {
        const DELAY_INTERVAL: time::Duration = time::Duration::milliseconds(500);
        const RETRY_INTERVAL: time::Duration = time::Duration::seconds(5);
        loop {
            match ctx.wait(self.client.get_block_number()).await? {
                Ok(head) => {
                    let head = L2BlockNumber(head.try_into().ok().context("overflow")?);
                    self.sync_state.set_main_node_block(head);
                    ctx.sleep(DELAY_INTERVAL).await?;
                }
                Err(err) => {
                    tracing::warn!("get_block_number(): {err}");
                    ctx.sleep(RETRY_INTERVAL).await?;
                }
            }
        }
    }

    /// Fetches consensus global configuration from the main node.
    #[tracing::instrument(skip_all)]
    async fn fetch_global_config(
        &self,
        ctx: &ctx::Ctx,
    ) -> ctx::Result<consensus_dal::GlobalConfig> {
        match ctx.wait(self.client.consensus_global_config()).await? {
            Ok(cfg) => {
                let cfg = cfg.context("main node is not running consensus component")?;
                return Ok(zksync_protobuf::serde::Deserialize {
                    deny_unknown_fields: false,
                }
                .proto_fmt(&cfg.0)
                .context("deserialize()")?);
            }
            // For non-whitelisted methods, proxyd returns HTTP 403 with MethodNotFound in the body.
            // For some stupid reason ClientError doesn't expose HTTP error codes.
            Err(ClientError::Transport(_)) => {}
            // For missing methods api server, returns HTTP 200 with MethodNotFound in the body.
            Err(ClientError::Call(err)) if err.code() == ErrorCode::MethodNotFound.code() => {}
            Err(err) => {
                return Err(err)
                    .context("consensus_global_config()")
                    .map_err(|err| err.into());
            }
        }
        tracing::info!("consensus_global_config() not found, calling consensus_genesis() instead");
        let genesis = ctx
            .wait(self.client.consensus_genesis())
            .await?
            .context("consensus_genesis()")?
            .context("main node is not running consensus component")?;
        Ok(consensus_dal::GlobalConfig {
            genesis: zksync_protobuf::serde::Deserialize {
                deny_unknown_fields: false,
            }
            .proto_fmt(&genesis.0)
            .context("deserialize()")?,
            registry_address: None,
            seed_peers: [].into(),
        })
    }

    #[tracing::instrument(skip_all)]
    async fn fetch_attestation_status(
        &self,
        ctx: &ctx::Ctx,
    ) -> ctx::Result<consensus_dal::AttestationStatus> {
        let status = ctx
            .wait(self.client.attestation_status())
            .await?
            .context("attestation_status()")?
            .context("main node is not runnign consensus component")?;
        Ok(zksync_protobuf::serde::Deserialize {
            deny_unknown_fields: false,
        }
        .proto_fmt(&status.0)
        .context("deserialize()")?)
    }

    /// Fetches (with retries) the given block from the main node.
    async fn fetch_block(
        &self,
        ctx: &ctx::Ctx,
        n: validator::BlockNumber,
    ) -> ctx::Result<FetchedBlock> {
        const RETRY_INTERVAL: time::Duration = time::Duration::seconds(5);
        let n = L2BlockNumber(n.0.try_into().context("overflow")?);
        METRICS.fetch_block.inc();
        loop {
            match ctx.wait(self.client.sync_l2_block(n, true)).await? {
                Ok(Some(block)) => return Ok(block.try_into()?),
                Ok(None) => {}
                Err(err) if is_retryable(&err) => {}
                Err(err) => Err(err).with_context(|| format!("client.sync_l2_block({n})"))?,
            }
            ctx.sleep(RETRY_INTERVAL).await?;
        }
    }

    /// Fetches blocks from the main node directly whenever the EN is lagging behind too much.
    pub(crate) async fn fallback_block_fetcher(
        &self,
        ctx: &ctx::Ctx,
        store: &Store,
    ) -> ctx::Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 30;
        scope::run!(ctx, |ctx, s| async {
            let (send, mut recv) = ctx::channel::bounded(MAX_CONCURRENT_REQUESTS);
            // TODO: metrics.
            s.spawn::<()>(async {
                let send = send;
                let is_lagging =
                    |main| main >= store.persisted().borrow().next() + FALLBACK_FETCHER_THRESHOLD;
                let mut next = store.next_block(ctx).await.wrap("next_block()")?;
                loop {
                    // Wait until p2p syncing is lagging.
                    self.sync_state
                        .wait_for_main_node_block(ctx, is_lagging)
                        .await?;
                    // Determine the next block to fetch and wait for it to be available.
                    next = next.max(store.next_block(ctx).await.wrap("next_block()")?);
                    self.sync_state
                        .wait_for_main_node_block(ctx, |main| main >= next)
                        .await?;
                    // Fetch the block asynchronously.
                    send.send(ctx, s.spawn(self.fetch_block(ctx, next))).await?;
                    next = next.next();
                }
            });
            loop {
                let block = recv.recv(ctx).await?;
                store
                    .queue_next_fetched_block(ctx, block.join(ctx).await?)
                    .await
                    .wrap("queue_next_fetched_block()")?;
            }
        })
        .await
    }

    /// Fetches blocks starting with `queue.next()`.
    async fn fetch_blocks(
        &self,
        ctx: &ctx::Ctx,
        queue: &mut storage::PayloadQueue,
    ) -> ctx::Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 30;
        let mut next = queue.next();
        scope::run!(ctx, |ctx, s| async {
            let (send, mut recv) = ctx::channel::bounded(MAX_CONCURRENT_REQUESTS);
            s.spawn::<()>(async {
                let send = send;
                loop {
                    self.sync_state
                        .wait_for_main_node_block(ctx, |main| main >= next)
                        .await?;
                    send.send(ctx, s.spawn(self.fetch_block(ctx, next))).await?;
                    next = next.next();
                }
            });
            loop {
                let block = recv.recv(ctx).await?.join(ctx).await?;
                queue.send(block).await.context("queue.send()")?;
            }
        })
        .await
    }
}
