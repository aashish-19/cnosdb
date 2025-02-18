pub mod deserialize;
pub mod replica_selection;
pub mod table_scan;
pub mod tag_scan;

use std::pin::Pin;
use std::task::{Context, Poll};

use datafusion::arrow::record_batch::RecordBatch;
use futures::future::BoxFuture;
use futures::{ready, FutureExt, Stream, StreamExt, TryFutureExt};
use meta::model::MetaRef;
use metrics::count::U64Counter;
use models::meta_data::{VnodeId, VnodeInfo, VnodeStatus};
use tracing::warn;
use tskv::reader::QueryOption;

use crate::errors::{CoordinatorError, CoordinatorResult};
use crate::service::CoordServiceMetrics;
use crate::SendableCoordinatorRecordBatchStream;

/// A fallible future that reads to a stream of [`RecordBatch`]
pub type VnodeOpenFuture =
    BoxFuture<'static, CoordinatorResult<SendableCoordinatorRecordBatchStream>>;
/// A fallible future that checks the vnode query operation is available
pub type CheckFuture = BoxFuture<'static, CoordinatorResult<()>>;

/// Generic API for connect a vnode and reading to a stream of [`RecordBatch`]
pub trait VnodeOpener: Unpin {
    /// Asynchronously open the specified vnode and return a stream of [`RecordBatch`]
    fn open(&self, vnode: &VnodeInfo, option: &QueryOption) -> CoordinatorResult<VnodeOpenFuture>;
}

pub struct CheckedCoordinatorRecordBatchStream<O: VnodeOpener> {
    opener: O,
    meta: MetaRef,
    vnode: VnodeInfo,
    option: QueryOption,
    state: StreamState,

    data_out: U64Counter,
}

impl<O: VnodeOpener> CheckedCoordinatorRecordBatchStream<O> {
    pub fn new(
        option: QueryOption,
        opener: O,
        meta: MetaRef,
        checker: CheckFuture,
        metrics: &CoordServiceMetrics,
    ) -> Self {
        let data_out = metrics.data_out(
            option.table_schema.tenant.as_str(),
            option.table_schema.db.as_str(),
        );

        Self {
            option,
            opener,
            meta,
            vnode: VnodeInfo::default(),
            state: StreamState::Check(checker),
            data_out,
        }
    }

    fn poll_inner(&mut self, cx: &mut Context<'_>) -> Poll<Option<CoordinatorResult<RecordBatch>>> {
        loop {
            match &mut self.state {
                StreamState::Check(checker) => {
                    // TODO record time used
                    match ready!(checker.try_poll_unpin(cx)) {
                        Ok(_) => {
                            self.vnode = self.option.split.pop_front().ok_or(
                                CoordinatorError::NoValidReplica {
                                    id: self.option.split.replica_id(),
                                },
                            )?;

                            self.state = StreamState::Idle;
                        }

                        Err(err) => return Poll::Ready(Some(Err(err))),
                    };
                }
                StreamState::Idle => {
                    // TODO record time used
                    let future = match self.opener.open(&self.vnode, &self.option) {
                        Ok(future) => future,
                        Err(err) => return Poll::Ready(Some(Err(err))),
                    };
                    self.state = StreamState::Open(future);
                }
                StreamState::Open(future) => {
                    // TODO record time used
                    match ready!(future.poll_unpin(cx)) {
                        Ok(stream) => {
                            self.state = StreamState::Scan(stream);
                        }
                        Err(err) => {
                            if let CoordinatorError::FailoverNode { id: _, ref error } = err {
                                if let Some(vnode) = self.option.split.pop_front() {
                                    warn!("failover reader try to read another vnode: {:?}, error: {}", vnode, error);
                                    self.vnode = vnode;
                                    self.state = StreamState::Idle;
                                } else {
                                    return Poll::Ready(Some(Err(err)));
                                }
                            } else {
                                return Poll::Ready(Some(Err(err)));
                            }
                        }
                    };
                }
                StreamState::Scan(stream) => return stream.poll_next_unpin(cx),
            }
        }
    }
}

impl<O: VnodeOpener> Stream for CheckedCoordinatorRecordBatchStream<O> {
    type Item = Result<RecordBatch, CoordinatorError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let poll = self.poll_inner(cx);
        if let Poll::Ready(Some(Ok(batch))) = &poll {
            self.data_out.inc(batch.get_array_memory_size() as u64);
        }

        if let Poll::Ready(Some(Err(err))) = &poll {
            if tskv::Error::vnode_broken_code(err.error_code().code()) {
                let id = self.vnode.id;
                let meta = self.meta.clone();
                let tenant = self.option.tenant_name();

                trace::warn!("updated vnode {} status broken", id);
                meta.try_change_local_vnode_status(&tenant, id, VnodeStatus::Broken);
                tokio::spawn(change_vnode_to_broken(tenant, id, meta));
            }
        }

        poll
    }
}

pub async fn change_vnode_to_broken(
    tenant: String,
    vnode_id: VnodeId,
    meta: MetaRef,
) -> CoordinatorResult<()> {
    let meta_client = meta.tenant_meta(&tenant).await;

    if let Some(meta_client) = meta_client {
        if let Some(mut all_info) = meta_client.get_vnode_all_info(vnode_id) {
            all_info.set_status(VnodeStatus::Broken);
            meta_client.update_vnode(&all_info).await?;

            return Ok(());
        }

        warn!(
            "Vnode not found: {}, when changing vnode state to broken.",
            vnode_id
        );

        return Ok(());
    }

    warn!(
        "Tenant not found: {}, when changing vnode state to broken.",
        tenant
    );

    Ok(())
}

enum StreamState {
    Check(CheckFuture),
    Idle,
    Open(VnodeOpenFuture),
    Scan(SendableCoordinatorRecordBatchStream),
}
