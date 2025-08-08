//! Distributed MQTT Broker gRPC Communication Layer
//!
//! Implements inter-node communication for clustered MQTT deployments with:
//! - High-performance message forwarding
//! - Cluster-wide operations and queries
//! - Fault-tolerant request handling
//!
//! ## Core Functionality
//! 1. ​**​gRPC Server​**​:
//!    - Manages incoming cluster RPC requests
//!    - Handles message forwarding between nodes
//!    - Processes cluster-wide queries (subscriptions, routes, etc.)
//!    - Automatic reconnection on failure
//!
//! 2. ​**​gRPC Client​**​:
//!    - Connection pooling and management
//!    - Request timeout handling
//!    - Concurrent request limiting
//!    - Message queue monitoring
//!
//! 3. ​**​Message Types​**​:
//!    - 22+ predefined message types for cluster operations
//!    - Serialization/deserialization via bincode
//!    - Support for custom binary data payloads
//!
//! ## Key Features
//! - Priority-based message channels
//! - Request/response tracking
//! - Broadcast operations to all nodes
//! - Optimistic response selection
//! - Configurable timeouts and limits:
//!   - `client_timeout`: Per-request timeout
//!   - `client_concurrency_limit`: Max concurrent requests
//!   - `chunk_size`: Message fragmentation threshold (2MB default)
//!
//! ## Implementation Details
//! - Uses Tokio for async I/O
//! - Atomic counters for request tracking
//! - Zero-copy message processing where possible
//! - Connection reuse for performance
//!
//! Typical Usage:
//! 1. Initialize gRPC server with `listen_and_serve()`
//! 2. Create client connections with `GrpcClient::new()`
//! 3. Send messages via:
//!    - Direct `send_message()` for point-to-point
//!    - `MessageBroadcaster` for cluster-wide ops
//! 4. Handle responses via `MessageReply` enum
//!
//! Note: All message types below 1000 are reserved for internal use.
//!
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::StreamExt;
use rust_box::handy_grpc::client::Mailbox;
use rust_box::handy_grpc::{
    client::{Client, DuplexMailbox},
    server::{server, Message as GrpcMessage},
    Priority,
};
use rust_box::mpsc::priority_channel as channel;
use scopeguard::defer;
use serde::{Deserialize, Serialize};

use crate::context::ServerContext;
use crate::types::{
    Addr, CleanStart, ClearSubscriptions, ClientId, From, HashMap, Id, IsAdmin, MsgID, NodeId,
    OfflineSession, Publish, Retain, Route, SessionStatus, SharedGroup, SubRelations, SubRelationsMap,
    SubsSearchParams, SubsSearchResult, SubscriptionClientIds, TopicFilter, TopicName,
};
use crate::utils::Counter;
use crate::Result;

#[derive(Clone)]
pub struct GrpcServer {
    scx: ServerContext,
}

impl GrpcServer {
    pub fn new(scx: ServerContext) -> Self {
        Self { scx }
    }

    pub async fn listen_and_serve(
        self,
        server_laddr: SocketAddr,
        reuseaddr: bool,
        reuseport: bool,
    ) -> Result<()> {
        let runner = async move {
            let (tx, mut rx) = channel::<Priority, GrpcMessage>(300_000);
            let recv_data_fut = async move {
                let exec = self.scx.get_exec("GRPC_SERVER_EXEC");
                while let Some((_, (data, reply_tx))) = rx.next().await {
                    #[cfg(feature = "stats")]
                    self.scx.stats.grpc_server_actives.inc();
                    let s = self.clone();
                    let recv_fut = async move {
                        let reply = s.on_recv_message(data).await;
                        if let Some(reply_tx) = reply_tx {
                            if let Err(e) = reply_tx.send(reply.map(|r| r.unwrap_or_default())) {
                                log::warn!("gRPC send result failure, {e:?}");
                            }
                        }
                        #[cfg(feature = "stats")]
                        s.scx.stats.grpc_server_actives.dec();
                    };
                    let _ = exec.spawn(recv_fut).await;
                }
                log::error!("Recv None");
            };

            let run_receiver_fut = async move {
                loop {
                    if let Err(e) = server(server_laddr, tx.clone())
                        .max_decoding_message_size(1024 * 1024 * 4)
                        .max_encoding_message_size(1024 * 1024 * 4)
                        .reuseaddr(reuseaddr)
                        .reuseport(reuseport)
                        .run()
                        .await
                    {
                        log::warn!("Run gRPC receiver error, {e:?}");
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            };
            futures::future::join(recv_data_fut, run_receiver_fut).await;
        };

        runner.await;

        Ok(())
    }

    async fn on_recv_message(&self, req: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let (typ, msg) = Message::decode(&req)?;
        // self.scx.stats.grpc_server_actives.inc();
        let reply = self.grpc_message_received(typ, msg).await?;
        // self.scx.stats.grpc_server_actives.dec();
        Ok(Some(reply.encode()?))
    }

    async fn grpc_message_received(&self, typ: MessageType, msg: Message) -> Result<MessageReply> {
        match (typ, msg) {
            (MESSAGE_TYPE_MESSAGE_GET, Message::MessageGet(client_id, topic_filter, group)) => {
                match self
                    .scx
                    .extends
                    .message_mgr()
                    .await
                    .get(&client_id, &topic_filter, group.as_ref())
                    .await
                {
                    Err(e) => Ok(MessageReply::Error(e.to_string())),
                    Ok(msgs) => Ok(MessageReply::MessageGet(msgs)),
                }
            }
            (_, msg) => self.scx.extends.hook_mgr().grpc_message_received(typ, msg).await,
        }
    }
}

pub type GrpcClients = Arc<HashMap<NodeId, (Addr, GrpcClient)>>;

#[derive(Clone)]
pub struct GrpcClient {
    mailbox: Mailbox,
    duplex_mailbox: DuplexMailbox,
    active_tasks: Arc<Counter>,
}

impl GrpcClient {
    //server_addr - ip:port, 127.0.0.1:6666
    #[inline]
    pub async fn new(
        server_addr: &str,
        client_timeout: Duration,
        client_concurrency_limit: usize,
    ) -> Result<Self> {
        log::info!(
            "GrpcClient::new server_addr: {server_addr}, client_concurrency_limit: {client_concurrency_limit}"
        );
        let mut c = Client::new(server_addr.into())
            .connect_timeout(client_timeout)
            .concurrency_limit(client_concurrency_limit)
            .chunk_size(1024 * 1024 * 2)
            .connect_lazy()?;
        let duplex_mailbox = c.duplex_transfer_start(100_000).await;
        let mailbox = c.transfer_start(100_000).await;
        let active_tasks = Arc::new(Counter::new());
        Ok(Self { mailbox, duplex_mailbox, active_tasks })
    }

    #[inline]
    pub fn active_tasks(&self) -> &Counter {
        self.active_tasks.as_ref()
    }

    #[inline]
    pub fn transfer_queue_len(&self) -> usize {
        self.mailbox.req_queue_len()
    }

    #[inline]
    pub async fn send_message(
        &mut self,
        typ: MessageType,
        msg: Message,
        timeout: Option<Duration>,
    ) -> Result<MessageReply> {
        let active_tasks = self.active_tasks.clone();
        active_tasks.inc();
        defer! {
            active_tasks.dec();
        }
        self._send_message(typ, msg, timeout).await
    }

    #[inline]
    async fn _send_message(
        &mut self,
        typ: MessageType,
        msg: Message,
        timeout: Option<Duration>,
    ) -> Result<MessageReply> {
        let req = msg.encode(typ)?;
        let reply = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, self.duplex_mailbox.send(req)).await??
        } else {
            self.duplex_mailbox.send(req).await?
        };
        MessageReply::decode(&reply)
    }

    #[inline]
    pub async fn notify(&mut self, typ: MessageType, msg: Message, timeout: Option<Duration>) -> Result<()> {
        let active_tasks = self.active_tasks.clone();
        active_tasks.inc();
        defer! {
            active_tasks.dec();
        }
        self._notify(typ, msg, timeout).await
    }
    #[inline]
    async fn _notify(&mut self, typ: MessageType, msg: Message, timeout: Option<Duration>) -> Result<()> {
        let req = msg.encode(typ)?;
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, self.mailbox.send(req)).await??;
        } else {
            self.mailbox.send(req).await?;
        };
        Ok(())
    }
}

///Reserved within 1000
pub type MessageType = u64;

pub const MESSAGE_TYPE_MESSAGE_GET: u64 = 22;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum Message {
    Forwards(From, Publish),
    ForwardsTo(From, Publish, SubRelations),
    Kick(Id, CleanStart, ClearSubscriptions, IsAdmin),
    GetRetains(TopicFilter),
    SubscriptionsSearch(SubsSearchParams),
    SubscriptionsGet(ClientId),
    RoutesGet(usize),
    RoutesGetBy(TopicFilter),
    NumberOfClients,
    NumberOfSessions,
    Online(ClientId),
    SessionStatus(ClientId),
    MessageGet(ClientId, TopicFilter, Option<SharedGroup>),
    Data(Vec<u8>),
}

impl Message {
    #[inline]
    pub fn encode(&self, typ: MessageType) -> Result<Vec<u8>> {
        Ok(bincode::serialize(&(typ, self))?)
    }
    #[inline]
    pub fn decode(data: &[u8]) -> Result<(MessageType, Message)> {
        Ok(bincode::deserialize::<(MessageType, Message)>(data)?)
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub enum MessageReply {
    Success,
    Forwards(SubRelationsMap, SubscriptionClientIds),
    Error(String),
    Kick(OfflineSession),
    GetRetains(Vec<(TopicName, Retain)>),
    SubscriptionsSearch(Vec<SubsSearchResult>),
    SubscriptionsGet(Option<Vec<SubsSearchResult>>),
    RoutesGet(Vec<Route>),
    RoutesGetBy(Vec<Route>),
    NumberOfClients(usize),
    NumberOfSessions(usize),
    Online(bool),
    SessionStatus(Option<SessionStatus>),
    MessageGet(Vec<(MsgID, From, Publish)>),
    Data(Vec<u8>),
}

impl MessageReply {
    #[inline]
    pub fn encode(&self) -> Result<Vec<u8>> {
        Ok(bincode::serialize(self)?)
    }
    #[inline]
    pub fn decode(data: &[u8]) -> Result<MessageReply> {
        Ok(bincode::deserialize::<MessageReply>(data)?)
    }
}

pub struct MessageSender {
    client: GrpcClient,
    msg_type: MessageType,
    msg: Message,
    timeout: Option<Duration>,
}

impl MessageSender {
    #[inline]
    pub fn new(client: GrpcClient, msg_type: MessageType, msg: Message, timeout: Option<Duration>) -> Self {
        Self { client, msg_type, msg, timeout }
    }

    #[inline]
    pub async fn send(mut self) -> Result<MessageReply> {
        match self.client.send_message(self.msg_type, self.msg, self.timeout).await {
            Ok(reply) => Ok(reply),
            Err(e) => {
                log::warn!("error sending message, {e:?}");
                Err(e)
            }
        }
    }

    #[inline]
    pub async fn notify(mut self) -> Result<()> {
        match self.client.notify(self.msg_type, self.msg, self.timeout).await {
            Ok(()) => Ok(()),
            Err(e) => {
                log::warn!("error notify message, {e:?}");
                Err(e)
            }
        }
    }
}

pub struct MessageBroadcaster {
    grpc_clients: GrpcClients,
    msg_type: MessageType,
    msg: Message,
    timeout: Option<Duration>,
}

impl MessageBroadcaster {
    #[inline]
    pub fn new(
        grpc_clients: GrpcClients,
        msg_type: MessageType,
        msg: Message,
        timeout: Option<Duration>,
    ) -> Self {
        assert!(!grpc_clients.is_empty(), "gRPC clients is empty!");
        Self { grpc_clients, msg_type, msg, timeout }
    }

    #[inline]
    pub async fn join_all(self) -> Vec<(NodeId, Result<MessageReply>)> {
        let msg = self.msg;
        let mut senders = Vec::new();
        for (id, (_, grpc_client)) in self.grpc_clients.iter() {
            let msg_type = self.msg_type;
            let msg = msg.clone();
            let fut =
                async move { (*id, grpc_client.clone().send_message(msg_type, msg, self.timeout).await) };
            senders.push(fut.boxed());
        }
        futures::future::join_all(senders).await
    }

    #[inline]
    pub async fn select_ok<R, F>(self, check_fn: F) -> Result<R>
    where
        R: std::any::Any + Send + Sync,
        F: Fn(MessageReply) -> Result<R> + Send + Sync,
    {
        let msg = self.msg;
        let mut senders = Vec::new();
        let max_idx = self.grpc_clients.len() - 1;
        for (i, (_, (_, grpc_client))) in self.grpc_clients.iter().enumerate() {
            if i == max_idx {
                senders.push(Self::send(grpc_client, self.msg_type, msg, self.timeout, &check_fn).boxed());
                break;
            } else {
                senders.push(
                    Self::send(grpc_client, self.msg_type, msg.clone(), self.timeout, &check_fn).boxed(),
                );
            }
        }
        let (reply, _) = futures::future::select_ok(senders).await?;
        Ok(reply)
    }

    #[inline]
    async fn send<R, F>(
        grpc_client: &GrpcClient,
        typ: MessageType,
        msg: Message,
        timeout: Option<Duration>,
        check_fn: &F,
    ) -> Result<R>
    where
        R: std::any::Any + Send + Sync,
        F: Fn(MessageReply) -> Result<R> + Send + Sync,
    {
        match grpc_client.clone().send_message(typ, msg, timeout).await {
            Ok(r) => {
                log::debug!("OK reply: {r:?}");
                check_fn(r)
            }
            Err(e) => {
                log::debug!("ERROR reply: {e:?}");
                Err(e)
            }
        }
    }
}
