#![deny(unsafe_code)]

use std::convert::From as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::channel::{mpsc, oneshot};
use futures::{SinkExt, StreamExt};
use serde_json::{self, json};

use rmqtt::{
    fitter::Fitter,
    hook::{Handler, HookResult, Parameter, Register, ReturnType, Type},
    plugin::{PackageInfo, Plugin},
    register,
    session::Session,
    types::DisconnectInfo,
    types::{ClientId, From, Publish, SessionSubMap, SessionSubs, TimestampMillis},
    utils::timestamp_millis,
    Result,
};

use rmqtt_storage::{init_db, DefaultStorageDB, List, Map, StorageType};

use config::PluginConfig;
use rmqtt::context::ServerContext;
use rmqtt::inflight::OutInflightMessage;
use rmqtt::macros::Plugin;
use rmqtt::session::SessionState;
use session::{Basic, StorageSessionManager, StoredSessionInfo, StoredSessionInfos};
use session::{StoredKey, BASIC, DISCONNECT_INFO, INFLIGHT_MESSAGES, LAST_TIME, SESSION_SUB_MAP};

mod config;
mod session;

enum RebuildChanType {
    Session(Session, Duration),
    Done(oneshot::Sender<()>),
}

type OfflineMessageOptionType = Option<(ClientId, From, Publish)>;

register!(StoragePlugin::new);

#[derive(Plugin)]
struct StoragePlugin {
    scx: ServerContext,
    cfg: Arc<PluginConfig>,
    storage_db: DefaultStorageDB,
    stored_session_infos: StoredSessionInfos,
    register: Box<dyn Register>,
    session_mgr: StorageSessionManager,
    rebuild_tx: mpsc::Sender<RebuildChanType>,
}

impl StoragePlugin {
    #[inline]
    async fn new<S: Into<String>>(scx: ServerContext, name: S) -> Result<Self> {
        let name = name.into();
        let mut cfg = scx.plugins.read_config_default::<PluginConfig>(&name)?;
        match cfg.storage.typ {
            #[cfg(feature = "sled")]
            StorageType::Sled => {
                cfg.storage.sled.path =
                    cfg.storage.sled.path.replace("{node}", &format!("{}", scx.node.id()));
            }
            #[cfg(feature = "redis")]
            StorageType::Redis => {
                cfg.storage.redis.prefix =
                    cfg.storage.redis.prefix.replace("{node}", &format!("{}", scx.node.id()));
            }
            #[cfg(feature = "redis-cluster")]
            StorageType::RedisCluster => {
                cfg.storage.redis_cluster.prefix =
                    cfg.storage.redis_cluster.prefix.replace("{node}", &format!("{}", scx.node.id()));
            }
        }

        log::info!("{name} StoragePlugin cfg: {cfg:?}");

        let storage_db = match init_db(&cfg.storage).await {
            Err(e) => {
                log::error!("{name} init storage db error, {e}");
                return Err(e);
            }
            Ok(db) => db,
        };

        let stored_session_infos = StoredSessionInfos::new();

        let register = scx.extends.hook_mgr().register();
        let session_mgr = StorageSessionManager::new(storage_db.clone(), stored_session_infos.clone());

        let cfg = Arc::new(cfg);
        let rebuild_tx = Self::start_local_runtime(scx.clone());
        Ok(Self { scx, cfg, storage_db, stored_session_infos, register, session_mgr, rebuild_tx })
    }

    async fn load_offline_session_infos(&mut self) -> Result<()> {
        log::info!("{:?} load_offline_session_infos ...", self.name());
        let storage_db = self.storage_db.clone();
        let mut iter_storage_db = storage_db.clone();
        //Load offline session information from the database
        let mut map_iter = iter_storage_db.map_iter().await?;
        while let Some(m) = map_iter.next().await {
            match m {
                Ok(m) => {
                    let id_key = StoredKey::from(map_stored_key_to_id_bytes(m.name()).to_vec());
                    log::debug!("map_stored_key: {id_key:?}");
                    let basic = match m.get::<_, Basic>(BASIC).await {
                        Err(e) => {
                            log::warn!("{id_key:?} load offline session basic info error, {e:?}");
                            if let Err(e) = storage_db.map_remove(m.name()).await {
                                log::warn!("{id_key:?} remove offline session info error, {e:?}");
                            }
                            continue;
                        }
                        Ok(None) => {
                            log::warn!("{id_key:?} offline session basic info is None");
                            if let Err(e) = storage_db.map_remove(m.name()).await {
                                log::warn!("{id_key:?} remove offline session info error, {e:?}");
                            }
                            continue;
                        }
                        Ok(Some(basic)) => basic,
                    };

                    log::debug!("basic: {basic:?}");
                    log::debug!("map key: {id_key:?}");
                    let mut s_info = StoredSessionInfo::from(id_key.clone(), basic);

                    match m.get::<_, TimestampMillis>(LAST_TIME).await {
                        Ok(Some(last_time)) => {
                            log::debug!("last_time: {last_time:?}");
                            s_info.set_last_time(last_time);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("{id_key:?} load offline session last time error, {e:?}");
                        }
                    }

                    match m.get::<_, SessionSubMap>(SESSION_SUB_MAP).await {
                        Ok(Some(subs)) => {
                            log::debug!("subs: {subs:?}");
                            s_info.set_subs(subs);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("{id_key:?} load offline session subscription info error, {e:?}");
                        }
                    }

                    match m.get::<_, DisconnectInfo>(DISCONNECT_INFO).await {
                        Ok(Some(disc_info)) => {
                            log::debug!("disc_info: {disc_info:?}");
                            s_info.set_disconnect_info(disc_info);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("{id_key:?} load offline session disconnect info error, {e:?}");
                        }
                    }

                    match m.get::<_, Vec<OutInflightMessage>>(INFLIGHT_MESSAGES).await {
                        Ok(Some(inflights)) => {
                            log::debug!("inflights len: {:?}", inflights.len());
                            s_info.inflight_messages = inflights;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log::warn!("{id_key:?} load offline session inflight messages error, {e:?}");
                        }
                    }

                    self.stored_session_infos.add(s_info);
                }
                Err(e) => {
                    log::warn!("load offline session info error, {e:?}");
                }
            }
        }
        drop(map_iter);

        let mut list_iter = iter_storage_db.list_iter().await?;
        while let Some(l) = list_iter.next().await {
            match l {
                Ok(l) => {
                    let id_key = StoredKey::from(list_stored_key_to_id_bytes(l.name()).to_vec());
                    log::debug!("list_stored_key, id_key: {id_key:?}");
                    match l.all::<OfflineMessageOptionType>().await {
                        Ok(offline_msgs) => {
                            log::debug!("{:?} offline_msgs len: {}", id_key, offline_msgs.len(),);
                            let ok =
                                self.stored_session_infos.set_offline_messages(id_key.clone(), offline_msgs);
                            log::debug!("{id_key:?} stored_session_infos, set_offline_messages res: {ok}");
                            if !ok {
                                if let Err(e) = storage_db.list_remove(l.name()).await {
                                    log::warn!("{id_key:?} remove offline messages error, {e:?}");
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("{id_key:?} load offline messages error, {e:?}");
                            if let Err(e) = storage_db.list_remove(l.name()).await {
                                log::warn!("{id_key:?} remove offline messages error, {e:?}");
                            }
                        }
                    }
                }
                Err(e) => {
                    log::warn!("load offline messages error, {e:?}");
                }
            }
        }
        drop(list_iter);

        for removed_key in self.stored_session_infos.retain_latests() {
            storage_db.map_remove(make_map_stored_key(removed_key.as_ref())).await?;
            storage_db.list_remove(make_list_stored_key(removed_key.as_ref())).await?;
        }
        log::info!("stored_session_infos len: {:?}", self.stored_session_infos.len());

        Ok(())
    }

    fn start_local_runtime(scx: ServerContext) -> mpsc::Sender<RebuildChanType> {
        let (tx, mut rx) = futures::channel::mpsc::channel::<RebuildChanType>(100_000);
        std::thread::spawn(move || {
            let local_rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime build failed");
            let local_set = tokio::task::LocalSet::new();

            local_set.block_on(&local_rt, async {
                let exec = scx.get_exec("SESSION_REBUILD_EXEC");
                while let Some(msg) = rx.next().await {
                    match msg {
                        RebuildChanType::Session(session, session_expiry_interval)  => {
                            match SessionState::offline_restart(session.clone(), session_expiry_interval).await {
                                Err(e) => {
                                    log::warn!("Rebuild offline sessions error, {e:?}");
                                },
                                Ok(msg_tx) => {
                                    let mut session_entry =
                                        scx.extends.shared().await.entry(session.id.clone());

                                    let id = session_entry.id().clone();
                                    let task_fut = async move {
                                        if let Err(e) = session_entry.set(session, msg_tx).await {
                                            log::warn!("{:?} Rebuild offline sessions error, {:?}", session_entry.id(), e);
                                        }
                                    };
                                    if let Err(e) = exec.spawn(task_fut).await {
                                        log::warn!("{:?} Rebuild offline sessions error, {:?}", id, e.to_string());
                                    }

                                    let completed_count = exec.completed_count().await;
                                    if completed_count > 0 && completed_count % 5000 == 0 {
                                        log::info!(
                                        "{:?} Rebuild offline sessions, completed_count: {}, active_count: {}, waiting_count: {}, rate: {:?}",
                                        id,
                                        exec.completed_count().await, exec.active_count(), exec.waiting_count(), exec.rate().await
                                    );
                                    }
                                }
                            }
                        },
                        RebuildChanType::Done(done_tx) => {
                            let _ = exec.flush().await;
                            let _ = done_tx.send(());
                            log::info!(
                                "Rebuild offline sessions, completed_count: {}, active_count: {}, waiting_count: {}, rate: {:?}",
                                exec.completed_count().await, exec.active_count(), exec.waiting_count(), exec.rate().await
                            );
                        }
                    }
                }
            });
            log::info!("Offline session rebuilding finished");
        });
        tx
    }
}

#[async_trait]
impl Plugin for StoragePlugin {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        log::info!("{} init", self.name());
        self.register
            .add(
                Type::BeforeStartup,
                Box::new(StorageHandler::new(
                    self.scx.clone(),
                    self.storage_db.clone(),
                    self.cfg.clone(),
                    self.stored_session_infos.clone(),
                    self.rebuild_tx.clone(),
                )),
            )
            .await;
        self.register
            .add(
                Type::OfflineMessage,
                Box::new(OfflineMessageHandler::new(self.cfg.clone(), self.storage_db.clone())),
            )
            .await;
        self.register
            .add(
                Type::OfflineInflightMessages,
                Box::new(OfflineMessageHandler::new(self.cfg.clone(), self.storage_db.clone())),
            )
            .await;

        self.load_offline_session_infos().await?;

        Ok(())
    }

    #[inline]
    async fn get_config(&self) -> Result<serde_json::Value> {
        Ok(self.cfg.to_json())
    }

    #[inline]
    async fn start(&mut self) -> Result<()> {
        log::info!("{} start", self.name());
        *self.scx.extends.session_mgr_mut().await = Box::new(self.session_mgr.clone());

        self.register.start().await;
        Ok(())
    }

    #[inline]
    async fn stop(&mut self) -> Result<bool> {
        log::warn!("{} stop, if the storage plugin is started, it cannot be stopped", self.name());
        Ok(false)
    }

    #[inline]
    async fn attrs(&self) -> serde_json::Value {
        async fn stats(storage_db: &DefaultStorageDB) -> (String, String, String, serde_json::Value) {
            let max_limit = 1000;
            let mut session_count = 0;
            let mut storage_db_map = storage_db.clone();
            {
                let now = std::time::Instant::now();
                let iter = storage_db_map.map_iter().await;
                if let Ok(mut iter) = iter {
                    while let Some(m) = iter.next().await {
                        if let Ok(m) = m {
                            log::debug!("map: {:?}", StoredKey::from(m.name().to_vec()));
                        }
                        session_count += 1;
                        if session_count >= max_limit {
                            break;
                        }
                    }
                }
                log::debug!("map_iter cost time: {:?}", now.elapsed());
            }

            let mut offline_session_count = 0;
            let mut offline_message_count = 0;
            let mut storage_db_list = storage_db.clone();
            {
                let now = std::time::Instant::now();
                let iter = storage_db_list.list_iter().await;
                if let Ok(mut iter) = iter {
                    while let Some(l) = iter.next().await {
                        if let Ok(mut l) = l {
                            log::debug!("list: {:?}", StoredKey::from(l.name().to_vec()));
                            if let Ok(mut l_iter) = l.iter::<OfflineMessageOptionType>().await {
                                while let Some(msg) = l_iter.next().await {
                                    if let Ok(Some(_)) = msg {
                                        offline_message_count += 1;
                                        if offline_message_count >= max_limit {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        offline_session_count += 1;
                        if offline_session_count >= max_limit {
                            break;
                        }
                    }
                }
                log::debug!("list_iter cost time: {:?}", now.elapsed());
            }
            let session_count = if session_count >= max_limit {
                format!("{session_count}+")
            } else {
                format!("{session_count}")
            };
            let offline_session_count = if offline_session_count >= max_limit {
                format!("{offline_session_count}+")
            } else {
                format!("{offline_session_count}")
            };
            let offline_message_count = if offline_message_count >= max_limit {
                format!("{offline_message_count}+")
            } else {
                format!("{offline_message_count}")
            };

            let storage_info = storage_db.info().await.unwrap_or_default();

            (session_count, offline_session_count, offline_message_count, storage_info)
        }

        let (session_count, offline_session_count, offline_message_count, storage_info) =
            match tokio::time::timeout(Duration::from_secs(1), stats(&self.storage_db)).await {
                Ok((session_count, offline_session_count, offline_message_count, storage_info)) => {
                    (session_count, offline_session_count, offline_message_count, storage_info)
                }
                Err(_) => ("Elapsed".into(), "Elapsed".into(), "Elapsed".into(), serde_json::Value::Null),
            };

        json!({
            "session_count": session_count,
            "offline_session_count": offline_session_count,
            "offline_message_count": offline_message_count,
            "storage_info": storage_info
        })
    }
}

struct OfflineMessageHandler {
    cfg: Arc<PluginConfig>,
    storage_db: DefaultStorageDB,
}

impl OfflineMessageHandler {
    fn new(cfg: Arc<PluginConfig>, storage_db: DefaultStorageDB) -> Self {
        Self { cfg, storage_db }
    }
}

#[async_trait]
impl Handler for OfflineMessageHandler {
    async fn hook(&self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        match param {
            Parameter::OfflineMessage(s, f, p) => {
                log::debug!(
                    "OfflineMessage storage_type: {:?}, from: {:?}, p: {:?}",
                    self.cfg.storage.typ,
                    f,
                    p
                );
                let list_stored_key = make_list_stored_key(s.id.to_string());
                let storage_db = self.storage_db.clone();
                let id = s.id.clone();
                let max_mqueue_len = s.listen_cfg().max_mqueue_len;
                let p = (*p).clone();
                let f = f.clone();
                tokio::spawn(async move {
                    match storage_db.list(list_stored_key.as_ref(), None).await {
                        Ok(offlines_list) => {
                            let res = offlines_list
                                .push_limit::<OfflineMessageOptionType>(
                                    &Some((id.client_id.clone(), f, p)),
                                    max_mqueue_len,
                                    true,
                                )
                                .await;
                            if let Err(e) = res {
                                log::warn!("{id:?} save offline messages error, {e}")
                            }
                        }
                        Err(e) => {
                            log::warn!("{id:?} save offline messages error, {e}")
                        }
                    }
                });
            }

            Parameter::OfflineInflightMessages(s, inflight_messages) => {
                log::debug!(
                    "OfflineInflightMessages storage_type: {:?}, inflight_messages len: {:?}",
                    self.cfg.storage.typ,
                    inflight_messages.len(),
                );
                let map_stored_key = make_map_stored_key(s.id.to_string());
                log::debug!("{:?} map_stored_key: {:?}", s.id, map_stored_key);

                let storage_db = self.storage_db.clone();
                let inflight_messages = inflight_messages.clone();
                let id = s.id.clone();
                tokio::spawn(async move {
                    match storage_db.map(map_stored_key.as_ref(), None).await {
                        Ok(m) => {
                            if let Err(e) = m.insert(INFLIGHT_MESSAGES, &inflight_messages).await {
                                log::warn!("{id:?} save offline inflight messages error, {e}")
                            }
                        }
                        Err(e) => {
                            log::warn!("{id:?} save offline inflight messages error, {e}")
                        }
                    }
                });
            }

            _ => {
                log::error!("unimplemented, {param:?}")
            }
        }
        (true, acc)
    }
}

struct StorageHandler {
    scx: ServerContext,
    storage_db: DefaultStorageDB,
    cfg: Arc<PluginConfig>,
    stored_session_infos: StoredSessionInfos,
    rebuild_tx: mpsc::Sender<RebuildChanType>,
}

impl StorageHandler {
    fn new(
        scx: ServerContext,
        storage_db: DefaultStorageDB,
        cfg: Arc<PluginConfig>,
        stored_session_infos: StoredSessionInfos,
        rebuild_tx: mpsc::Sender<RebuildChanType>,
    ) -> Self {
        Self { scx, storage_db, cfg, stored_session_infos, rebuild_tx }
    }

    //Rebuild offline sessions.
    async fn rebuild_offline_sessions(&self, rebuild_done_tx: oneshot::Sender<()>) {
        let mut offline_sessions_count = 0;
        for mut entry in self.stored_session_infos.iter_mut() {
            let (_, storeds) = entry.pair_mut();
            if let Some(stored) = storeds.iter_mut().next() {
                let id = stored.basic.id.clone();

                let listen_cfg =
                    if let Some(listen_cfg) = self.scx.listen_cfgs.get(&id.lid).map(|c| c.value().clone()) {
                        listen_cfg
                    } else {
                        log::warn!("tcp listener config is not found, local addr is {:?}", id.local_addr);
                        continue;
                    };

                log::info!("{id:?} listen_cfg: {listen_cfg:?}");

                //create fitter
                let fitter = self.scx.extends.fitter_mgr().await.create(
                    stored.basic.conn_info.clone(),
                    id.clone(),
                    listen_cfg.clone(),
                );

                //check session expiry interval
                let session_expiry_interval = session_expiry_interval(
                    fitter.as_ref(),
                    stored.disconnect_info.as_ref(),
                    stored.last_time,
                )
                .await;
                log::debug!("{id:?} session_expiry_interval: {session_expiry_interval:?}");
                if session_expiry_interval <= 0 {
                    log::debug!(
                        "{:?} session is expiry, {:?}, id_key: {:?}, {:?}, {:?}",
                        id,
                        session_expiry_interval,
                        stored.id_key,
                        make_map_stored_key(stored.id_key.as_ref()),
                        make_list_stored_key(stored.id_key.as_ref())
                    );
                    let storage_db = self.storage_db.clone();
                    if let Err(e) = storage_db.map_remove(make_map_stored_key(stored.id_key.as_ref())).await {
                        log::warn!("{id:?} remove map error, {e:?}");
                    }
                    if let Err(e) = storage_db.list_remove(make_list_stored_key(stored.id_key.as_ref())).await
                    {
                        log::warn!("{id:?} remove list error, {e:?}");
                    }
                    //session is expiry
                    continue;
                }
                offline_sessions_count += 1;

                if stored.disconnect_info.is_none() {
                    stored.disconnect_info = Some(DisconnectInfo::new(stored.last_time));
                }

                let max_inflight = fitter.max_inflight();
                let max_mqueue_len = fitter.max_mqueue_len();
                let subs = stored.subs.take().map(SessionSubs::from).unwrap_or_else(SessionSubs::new);

                let session = match Session::new(
                    id.clone(),
                    self.scx.clone(),
                    max_mqueue_len,
                    listen_cfg,
                    fitter,
                    None,
                    max_inflight,
                    stored.basic.created_at,
                    stored.basic.conn_info.clone(),
                    false,
                    false,
                    false,
                    stored.basic.connected_at,
                    subs,
                    stored.disconnect_info.take(),
                    None,
                )
                .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("rebuild session offline message error, create session error, {e:?}");
                        continue;
                    }
                };

                let deliver_queue = session.deliver_queue();
                for item in stored.offline_messages.drain(..) {
                    if let Err((f, p)) = deliver_queue.push(item) {
                        log::warn!("rebuild session offline message error, deliver queue is full, from: {f:?}, publish: {p:?}");
                    }
                }

                let out_inflight = session.out_inflight();
                for item in stored.inflight_messages.drain(..) {
                    out_inflight.write().await.push_back(item);
                }

                if let Err(e) = self
                    .rebuild_tx
                    .clone()
                    .send(RebuildChanType::Session(
                        session,
                        Duration::from_millis(session_expiry_interval as u64),
                    ))
                    .await
                {
                    log::error!("rebuild offline sessions error, {e:?}");
                }
            }
        }
        log::info!("offline_sessions_count: {offline_sessions_count}");
        let _ = self.rebuild_tx.clone().send(RebuildChanType::Done(rebuild_done_tx)).await;
    }
}

#[async_trait]
impl Handler for StorageHandler {
    async fn hook(&self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        match param {
            Parameter::BeforeStartup => {
                log::info!(
                    "BeforeStartup storage_type: {:?}, stored_session_infos len: {}",
                    self.cfg.storage.typ,
                    self.stored_session_infos.len()
                );
                let (rebuild_done_tx, rebuild_done_rx) = oneshot::channel::<()>();
                self.rebuild_offline_sessions(rebuild_done_tx).await;
                let _ = rebuild_done_rx.await;
            }
            _ => {
                log::error!("unimplemented, {param:?}")
            }
        }
        (true, acc)
    }
}

#[inline]
async fn session_expiry_interval(
    fitter: &dyn Fitter,
    disconnect_info: Option<&DisconnectInfo>,
    last_time: TimestampMillis,
) -> TimestampMillis {
    let disconnected_at = disconnect_info.map(|d| d.disconnected_at).unwrap_or_default();
    let disconnected_at = if disconnected_at <= 0 { last_time } else { disconnected_at };
    fitter.session_expiry_interval(disconnect_info.and_then(|d| d.mqtt_disconnect.as_ref())).as_millis()
        as i64
        - (timestamp_millis() - disconnected_at)
}

#[inline]
pub(crate) fn make_map_stored_key<T: AsRef<[u8]>>(id: T) -> StoredKey {
    let mut key = Vec::from("map-");
    key.extend_from_slice(id.as_ref());
    Bytes::from(key)
}

#[inline]
pub(crate) fn map_stored_key_to_id_bytes(stored_key: &[u8]) -> &[u8] {
    if stored_key.starts_with(b"map-") {
        stored_key[4..].as_ref()
    } else {
        stored_key
    }
}

#[inline]
pub(crate) fn make_list_stored_key<T: AsRef<[u8]>>(id: T) -> StoredKey {
    let mut key = Vec::from("list-");
    key.extend_from_slice(id.as_ref());
    Bytes::from(key)
}

#[inline]
pub(crate) fn list_stored_key_to_id_bytes(stored_key: &[u8]) -> &[u8] {
    if stored_key.starts_with(b"list-") {
        stored_key[5..].as_ref()
    } else {
        stored_key
    }
}
