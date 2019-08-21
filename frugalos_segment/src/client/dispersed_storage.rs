#![allow(clippy::needless_pass_by_value)]
use cannyls::deadline::Deadline;
use cannyls::lump::LumpData;
use cannyls_rpc::Client as CannyLsClient;
use cannyls_rpc::DeviceId;
use ecpool::liberasurecode::LibErasureCoderBuilder;
use ecpool::ErasureCoderPool;
use fibers::time::timer;
use fibers_rpc::client::ClientServiceHandle as RpcServiceHandle;
use frugalos_core::tracer::SpanExt;
use frugalos_raft::NodeId;
use futures::{self, Async, Future, Poll};
use libfrugalos::entity::object::ObjectVersion;
use rustracing::tag::{StdTag, Tag};
use rustracing_jaeger::span::{Span, SpanHandle};
use slog::Logger;
use std::mem;
use std::sync::Arc;
use std::time::Duration;
use trackable::error::ErrorKindExt;

use client::ec::{build_ec, ErasureCoder};
use client::storage::{append_checksum, verify_and_remove_checksum, MaybeFragment, PutAll};
use config::{
    CannyLsClientConfig, ClusterConfig, ClusterMember, DispersedClientConfig, DispersedConfig,
    Participants,
};
use metrics::{DispersedClientMetrics, PutAllMetrics};
use util::{BoxFuture, Phase};
use {Error, ErrorKind, Result};

#[derive(Clone)]
pub struct DispersedClient {
    logger: Logger,
    metrics: DispersedClientMetrics,
    cluster: Arc<ClusterConfig>,
    config: DispersedConfig,
    client_config: DispersedClientConfig,
    data_fragments: usize,
    ec: ErasureCoder,
    rpc_service: RpcServiceHandle,
}
impl DispersedClient {
    pub fn new(
        logger: Logger,
        metrics: DispersedClientMetrics,
        cluster: ClusterConfig,
        config: DispersedConfig,
        client_config: DispersedClientConfig,
        rpc_service: RpcServiceHandle,
        ec: Option<ErasureCoder>,
    ) -> Self {
        let parity_fragments = config.tolerable_faults as usize;
        let data_fragments = config.fragments as usize - parity_fragments;
        let ec = ec.unwrap_or_else(|| build_ec(data_fragments, parity_fragments));
        DispersedClient {
            logger,
            metrics,
            cluster: Arc::new(cluster),
            config,
            client_config,
            ec,
            data_fragments,
            rpc_service,
        }
    }
    pub fn get_fragment(
        self,
        local_node: NodeId,
        version: ObjectVersion,
    ) -> ReconstructDispersedFragment {
        let candidates = self
            .cluster
            .candidates(version)
            .cloned()
            .collect::<Vec<_>>();
        let participants = Participants::dispersed(&candidates, self.config.fragments());
        let missing_index = participants.fragment_index(&local_node);
        let mut spares = participants.spares(&local_node);
        spares.reverse();

        // let spares = self.cluster
        //     .members
        //     .iter()
        //     .filter(|m| m.node != local_node)
        //     .cloned()
        //     .collect::<Vec<_>>();
        debug!(
            self.logger,
            "get_fragment: version={:?}, missing_index={:?}, spares={:?}",
            version,
            missing_index,
            spares
        );

        // rand::thread_rng().shuffle(&mut spares);
        let dummy: BoxFuture<_> = Box::new(futures::finished(None));
        let future = CollectFragments {
            logger: self.logger.clone(),
            futures: vec![dummy],
            fragments: Vec::new(),
            data_fragments: self.data_fragments,
            spares,
            version,
            deadline: Deadline::Infinity,
            cannyls_config: self.client_config.cannyls.clone(),
            rpc_service: self.rpc_service,
            parent: Span::inactive().handle(), // TODO
            timeout: None,
            next_timeout_duration: self.client_config.get_timeout,
        };
        ReconstructDispersedFragment {
            phase: Phase::A(future),
            ec: self.ec.clone(),
            missing_index,
        }
    }
    pub fn get(
        self,
        version: ObjectVersion,
        deadline: Deadline,
        parent: SpanHandle,
    ) -> BoxFuture<Vec<u8>> {
        let mut spares = self
            .cluster
            .candidates(version)
            .cloned()
            .collect::<Vec<_>>();
        spares.reverse();
        // rand::thread_rng().shuffle(&mut spares);
        let dummy: BoxFuture<_> = Box::new(futures::finished(None));

        let span = parent.child("get_content", |span| {
            span.tag(StdTag::component(module_path!()))
                .tag(Tag::new("object.version", version.0 as i64))
                .tag(Tag::new("storage.type", "dispersed"))
                .start()
        });
        let future = CollectFragments {
            logger: self.logger.clone(),
            futures: vec![dummy],
            fragments: Vec::new(),
            data_fragments: self.data_fragments,
            spares,
            version,
            deadline,
            cannyls_config: self.client_config.cannyls.clone(),
            rpc_service: self.rpc_service,
            parent: span.handle(),
            timeout: Some(timer::timeout(self.client_config.get_timeout)),
            next_timeout_duration: self.client_config.get_timeout,
        };
        Box::new(DispersedGet {
            phase: Phase::A(future),
            ec: self.ec.clone(),
            span,
        })
    }
    pub fn put(
        self,
        version: ObjectVersion,
        content: Vec<u8>,
        deadline: Deadline,
        parent: SpanHandle,
    ) -> BoxFuture<()> {
        let span = parent.child("put_content", |span| {
            span.tag(StdTag::component(module_path!()))
                .tag(Tag::new("object.version", version.0 as i64))
                .tag(Tag::new("storage.type", "dispersed"))
                .start()
        });

        let mut child = span.child("ec_encode", |span| {
            span.tag(StdTag::component(module_path!())).start()
        });
        let future = self
            .ec
            .encode(content)
            .map_err(|e| track!(Error::from(e)))
            .then(move |result| {
                if let Err(ref e) = result {
                    child.set_tag(StdTag::error);
                    child.log(|log| {
                        log.error().message(e.to_string());
                    });
                }
                result
            });
        Box::new(DispersedPut {
            // NOTE: 他のメトリクスを追加するタイミングで `DispersedPut` 用の metrics に変更する
            metrics: self.metrics.put_all,
            cluster: self.cluster.clone(),
            version,
            deadline,
            cannyls_config: self.client_config.cannyls.clone(),
            data_fragments: self.data_fragments,
            rpc_service: self.rpc_service,
            phase: Phase::A(Box::new(future)),
            parent: span,
        })
    }
}

pub struct DispersedPut {
    metrics: PutAllMetrics,
    cluster: Arc<ClusterConfig>,
    version: ObjectVersion,
    deadline: Deadline,
    cannyls_config: CannyLsClientConfig,
    data_fragments: usize,
    rpc_service: RpcServiceHandle,
    phase: Phase<BoxFuture<Vec<Vec<u8>>>, PutAll>,
    parent: Span,
}
impl Future for DispersedPut {
    type Item = ();
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        while let Async::Ready(phase) = track!(self.phase.poll().map_err(Error::from))? {
            let next = match phase {
                Phase::A(fragments) => {
                    let parent = self.parent.handle();
                    let version = self.version;
                    let deadline = self.deadline;
                    let cannyls_config = self.cannyls_config.clone();
                    let rpc_service = self.rpc_service.clone();
                    let futures = self
                        .cluster
                        .candidates(self.version)
                        .zip(fragments.into_iter())
                        .map(move |(m, mut content)| {
                            append_checksum(&mut content);
                            let client = CannyLsClient::new(m.node.addr, rpc_service.clone());
                            let mut request = client.request();
                            request.rpc_options(cannyls_config.rpc_options());

                            let device_id = m.device.clone();
                            let lump_id = m.make_lump_id(version);
                            let data = match track!(LumpData::new(content)) {
                                Ok(data) => data,
                                Err(error) => {
                                    let future: BoxFuture<_> =
                                        Box::new(futures::failed(Error::from(error)));
                                    return future;
                                }
                            };

                            let mut span = parent.child("put_fragment", |span| {
                                span.tag(StdTag::component(module_path!()))
                                    .tag(StdTag::span_kind("client"))
                                    .tag(StdTag::peer_ip(m.node.addr.ip()))
                                    .tag(StdTag::peer_port(m.node.addr.port()))
                                    .tag(Tag::new("node", m.node.local_id.to_string()))
                                    .tag(Tag::new("device.id", device_id.clone()))
                                    .tag(Tag::new("lump.id", lump_id.to_string()))
                                    .tag(Tag::new("lump.bytes", data.as_bytes().len() as i64))
                                    .start()
                            });
                            let future: BoxFuture<_> = Box::new(
                                request
                                    .deadline(deadline)
                                    .max_queue_len(cannyls_config.device_max_queue_len)
                                    .put_lump(DeviceId::new(device_id), lump_id, data)
                                    .map(|_is_new| ())
                                    .map_err(|e| track!(Error::from(e)))
                                    .then(move |result| {
                                        if let Err(ref e) = result {
                                            span.log_error(e);
                                        }
                                        result
                                    }),
                            );
                            future
                        });
                    Phase::B(track!(PutAll::new(
                        self.metrics.clone(),
                        futures,
                        self.data_fragments
                    ))?)
                }
                Phase::B(()) => {
                    return Ok(Async::Ready(()));
                }
            };
            self.phase = next;
        }
        Ok(Async::NotReady)
    }
}

pub struct DispersedGet {
    phase: Phase<CollectFragments, BoxFuture<Vec<u8>>>,
    ec: ErasureCoderPool<LibErasureCoderBuilder>,
    span: Span,
}
impl Future for DispersedGet {
    type Item = Vec<u8>;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        while let Async::Ready(phase) = track!(self.phase.poll().map_err(Error::from))? {
            let next = match phase {
                Phase::A(fragments) => {
                    let mut child = self.span.child("ec_decode", |span| {
                        span.tag(StdTag::component(module_path!()))
                            .tag(Tag::new(
                                "fragments.bytes",
                                fragments.iter().map(Vec::len).sum::<usize>() as i64,
                            ))
                            .start()
                    });
                    let future: BoxFuture<_> = Box::new(
                        self.ec
                            .decode(fragments)
                            .map_err(|e| track!(Error::from(e)))
                            .then(move |result| {
                                if let Err(ref e) = result {
                                    child.set_tag(StdTag::error);
                                    child.log(|log| {
                                        log.error().message(e.to_string());
                                    });
                                }
                                result
                            }),
                    );
                    Phase::B(future)
                }
                Phase::B(content) => return Ok(Async::Ready(content)),
            };
            self.phase = next;
        }
        Ok(Async::NotReady)
    }
}

struct CollectFragments {
    logger: Logger,
    futures: Vec<BoxFuture<Option<Vec<u8>>>>,
    fragments: Vec<Vec<u8>>,
    data_fragments: usize,
    spares: Vec<ClusterMember>,
    version: ObjectVersion,
    deadline: Deadline,
    cannyls_config: CannyLsClientConfig,
    rpc_service: RpcServiceHandle,
    parent: SpanHandle,

    // フラグメント群の取得のタイムアウト時間
    //
    // 基本的にはディスクI/O負荷を抑えるために、
    // 必要最小限のフラグメントのみを取得しようと試みるが、
    // タイムアウト時間に達した場合には、
    // 極度に遅延しているディスクがあるものと判断して、
    // 取得候補を増やすことになる。
    //
    // TODO: リクエスト数が多い場合にはRPCレベルでの遅延防止処理がある旨を記述する
    //
    // TODO: `deadline`を考慮した値を使うようにする
    timeout: Option<timer::Timeout>,

    /// How long to wait before aborting the next get operation.
    next_timeout_duration: Duration,
}
impl CollectFragments {
    fn fill_shortage_from_spare(&mut self, mut force: bool) -> Result<()> {
        while force || self.futures.len() + self.fragments.len() < self.data_fragments {
            force = false;

            let m = track!(self
                           .spares
                           .pop()
                           .ok_or_else(|| {
                               let cause = format!(
                                   "There are no enough fragments (Detail: futures.len({}) + fragments.len({}) < data_fragments({}))",
                                   self.futures.len(),
                                   self.fragments.len(),
                                   self.data_fragments
                               );
                               Error::from(ErrorKind::Corrupted.cause(cause))
                           }))?;

            let client = CannyLsClient::new(m.node.addr, self.rpc_service.clone());
            let lump_id = m.make_lump_id(self.version);
            debug!(
                self.logger,
                "[CollectFragments({},{},{}/{})] candidate={:?}, lump_id={:?}",
                self.spares.len(),
                self.futures.len(),
                self.fragments.len(),
                self.data_fragments,
                m.node,
                lump_id
            );
            let mut span = self.parent.child("collect_fragment", |span| {
                span.tag(StdTag::component(module_path!()))
                    .tag(StdTag::span_kind("client"))
                    .tag(StdTag::peer_ip(m.node.addr.ip()))
                    .tag(StdTag::peer_port(m.node.addr.port()))
                    .tag(Tag::new("device", m.device.clone()))
                    .tag(Tag::new("lump", format!("{:?}", lump_id)))
                    .start()
            });

            let mut request = client.request();
            request.rpc_options(self.cannyls_config.rpc_options());

            let future = request
                .deadline(self.deadline)
                .get_lump(DeviceId::new(m.device), lump_id)
                .then(move |result| {
                    if let Err(ref e) = result {
                        span.log_error(e);
                    }
                    result
                });
            let future: BoxFuture<_> = Box::new(future.map_err(|e| track!(Error::from(e))));
            self.futures.push(future);
        }
        Ok(())
    }
}
impl Future for CollectFragments {
    type Item = Vec<Vec<u8>>;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let mut i = 0;
            while i < self.futures.len() {
                match track!(self.futures[i].poll()) {
                    Err(e) => {
                        self.futures.swap_remove(i);
                        debug!(self.logger, "[CollectFragments] Error: {}", e);
                        track!(self.fill_shortage_from_spare(false), "Last error: {}", e)?;
                    }
                    Ok(Async::NotReady) => {
                        i += 1;
                    }
                    Ok(Async::Ready(fragment)) => {
                        self.futures.swap_remove(i);
                        if let Some(mut fragment) = fragment {
                            if let Err(e) = track!(verify_and_remove_checksum(&mut fragment)) {
                                // TODO: Add protection for log overflow
                                warn!(self.logger, "[CollectFragments] Corrupted fragment: {}", e);
                                track!(self.fill_shortage_from_spare(false))?;
                            } else {
                                self.fragments.push(fragment);
                            }
                        } else {
                            debug!(self.logger, "[CollectFragments] NotFound");
                            track!(self.fill_shortage_from_spare(false))?;
                        }
                    }
                }
            }
            if self.fragments.len() == self.data_fragments {
                return Ok(Async::Ready(mem::replace(&mut self.fragments, Vec::new())));
            }
            if let Ok(Async::Ready(Some(()))) = self.timeout.poll() {
                // TODO: ログは出さなくする(かわりにprometheusを使う)
                info!(
                    self.logger,
                    "Collecting fragments timeout expired: add new candidate. next_timeout={:?}",
                    self.next_timeout_duration
                );
                self.timeout = None;
                if !self.spares.is_empty() {
                    if let Err(e) = track!(self.fill_shortage_from_spare(true)) {
                        warn!(self.logger, "{}", e);
                    } else {
                        self.timeout = Some(timer::timeout(self.next_timeout_duration));
                        continue;
                    }
                }
            }
            break;
        }
        Ok(Async::NotReady)
    }
}

/// Reconstructs original data from dispersed fragments even if
/// a focusing node loses its data fragment.
pub struct ReconstructDispersedFragment {
    /// The processing order of futures
    phase: Phase<CollectFragments, BoxFuture<Vec<u8>>>,

    /// A thread pool of encoders(by erasure code)
    ec: ErasureCoderPool<LibErasureCoderBuilder>,

    /// The index of a focusing node.
    /// None represents that there is no missing index.
    missing_index: Option<usize>,
}
impl Future for ReconstructDispersedFragment {
    type Item = MaybeFragment;
    type Error = Error;
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.missing_index.is_none() {
            return Ok(Async::Ready(MaybeFragment::NotParticipant));
        }

        let missing_index = self.missing_index.expect("never fails");

        while let Async::Ready(phase) = track!(self.phase.poll().map_err(Error::from))? {
            let next = match phase {
                Phase::A(fragments) => {
                    let future = self.ec.reconstruct(missing_index, fragments);
                    let future: BoxFuture<_> = Box::new(future.map_err(|e| track!(Error::from(e))));
                    Phase::B(future)
                }
                Phase::B(fragment) => return Ok(Async::Ready(MaybeFragment::Fragment(fragment))),
            };
            self.phase = next;
        }
        Ok(Async::NotReady)
    }
}
