#![deny(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{self, json};
use tokio::sync::RwLock;

use rmqtt::{
    context::ServerContext,
    hook::{Handler, HookResult, Parameter, Register, ReturnType, Type},
    macros::Plugin,
    plugin::Plugin,
    register, Result,
};

#[cfg(feature = "ram")]
use ram::RamRetainer;
#[cfg(any(feature = "sled", feature = "redis"))]
use rmqtt_storage::{init_db, StorageType};

use config::{Config, PluginConfig};
use rmqtt::plugin::PackageInfo;
use rmqtt::retain::RetainStorage;

mod config;
#[cfg(feature = "ram")]
mod ram;
#[cfg(any(feature = "sled", feature = "redis"))]
mod storage;

register!(RetainerPlugin::new);

#[derive(Plugin)]
struct RetainerPlugin {
    scx: ServerContext,
    register: Box<dyn Register>,
    cfg: Arc<RwLock<PluginConfig>>,
    retainer: Retainer,
    support_cluster: bool,
    retain_enable: Arc<AtomicBool>,
}

impl RetainerPlugin {
    #[inline]
    async fn new<N: Into<String>>(scx: ServerContext, name: N) -> Result<Self> {
        let name = name.into();

        let cfg = scx.plugins.read_config::<PluginConfig>(&name)?;
        log::info!("{name} RetainerPlugin cfg: {cfg:?}");
        let register = scx.extends.hook_mgr().register();
        let cfg = Arc::new(RwLock::new(cfg));
        let retain_enable = Arc::new(AtomicBool::new(false));

        let (retainer, support_cluster) = match &mut cfg.write().await.storage {
            #[cfg(feature = "ram")]
            Config::Ram => (Retainer::Ram(RamRetainer::new(cfg.clone(), retain_enable.clone())), false),
            #[cfg(any(feature = "sled", feature = "redis"))]
            Config::Storage(s_cfg) => {
                let node_id = scx.node.id();
                let support_cluster = match s_cfg.typ {
                    #[cfg(feature = "sled")]
                    StorageType::Sled => {
                        s_cfg.sled.path = s_cfg.sled.path.replace("{node}", &format!("{node_id}"));
                        false
                    }
                    #[cfg(feature = "redis")]
                    StorageType::Redis => {
                        s_cfg.redis.prefix = s_cfg.redis.prefix.replace("{node}", &format!("{node_id}"));
                        true
                    }
                    #[allow(unreachable_patterns)]
                    _ => return Err(anyhow::anyhow!("unsupported storage type")),
                };
                let storage_db = init_db(s_cfg).await?;
                (
                    Retainer::Storage(
                        storage::Retainer::new(node_id, cfg.clone(), storage_db, retain_enable.clone())
                            .await?,
                    ),
                    support_cluster,
                )
            }
        };

        Ok(Self { scx, register, cfg, retainer, support_cluster, retain_enable })
    }
}

#[async_trait]
impl Plugin for RetainerPlugin {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        log::info!("{} init", self.name());
        self.register
            .add(
                Type::BeforeStartup,
                Box::new(RetainHandler::new(
                    self.scx.clone(),
                    self.support_cluster,
                    self.retain_enable.clone(),
                )),
            )
            .await;

        let retainer = self.retainer.clone();
        //I run every 10 seconds
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let removeds = retainer.remove_expired_messages().await;
                if removeds > 0 {
                    log::debug!(
                        "{:?} remove_expired_messages, removed count: {}",
                        std::thread::current().id(),
                        removeds
                    );
                }
            }
        });

        Ok(())
    }

    #[inline]
    async fn get_config(&self) -> Result<serde_json::Value> {
        self.cfg.read().await.to_json()
    }

    #[inline]
    async fn load_config(&mut self) -> Result<()> {
        let new_cfg = self.scx.plugins.read_config::<PluginConfig>(self.name())?;
        *self.cfg.write().await = new_cfg;
        log::debug!("load_config ok,  {:?}", self.cfg);
        Ok(())
    }

    #[inline]
    async fn start(&mut self) -> Result<()> {
        log::info!("{} start", self.name());
        let r: Box<dyn RetainStorage> = match self.retainer.clone() {
            #[cfg(feature = "ram")]
            Retainer::Ram(r) => Box::new(r),
            #[cfg(any(feature = "sled", feature = "redis"))]
            Retainer::Storage(r) => Box::new(r),
        };
        *self.scx.extends.retain_mut().await = r;
        self.register.start().await;

        Ok(())
    }

    #[inline]
    async fn stop(&mut self) -> Result<bool> {
        log::warn!("{} stop, the default Retainer plug-in, it cannot be stopped", self.name());
        //self.register.stop().await;
        Ok(false)
    }

    #[inline]
    async fn attrs(&self) -> serde_json::Value {
        self.retainer.info().await
    }
}

struct RetainHandler {
    scx: ServerContext,
    support_cluster: bool,
    retain_enable: Arc<AtomicBool>,
}

impl RetainHandler {
    fn new(scx: ServerContext, support_cluster: bool, retain_enable: Arc<AtomicBool>) -> Self {
        Self { scx, support_cluster, retain_enable }
    }
}

#[async_trait]
impl Handler for RetainHandler {
    async fn hook(&self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        match param {
            Parameter::BeforeStartup => {
                let grpc_enable = self.scx.extends.shared().await.grpc_enable();
                if grpc_enable && !self.support_cluster {
                    log::warn!("{ERR_NOT_SUPPORTED}");
                    self.retain_enable.store(false, Ordering::SeqCst);
                } else {
                    self.retain_enable.store(true, Ordering::SeqCst);
                }
            }
            _ => {
                log::error!("unimplemented, {param:?}")
            }
        }
        (true, acc)
    }
}

#[derive(Clone)]
enum Retainer {
    #[cfg(feature = "ram")]
    Ram(RamRetainer),
    #[cfg(any(feature = "sled", feature = "redis"))]
    Storage(storage::Retainer),
}

impl Retainer {
    async fn remove_expired_messages(&self) -> usize {
        match self {
            #[cfg(feature = "ram")]
            Retainer::Ram(r) => r.remove_expired_messages().await,
            #[cfg(any(feature = "sled", feature = "redis"))]
            Retainer::Storage(_r) => 0,
        }
    }

    async fn info(&self) -> serde_json::Value {
        match self {
            #[cfg(feature = "ram")]
            Retainer::Ram(r) => {
                let msg_max = r.max().await;
                let msg_count = r.count().await;
                let topic_nodes = r.inner.messages.read().await.nodes_size();
                let topic_values = r.inner.messages.read().await.values_size();
                json!({
                    "storage_engine": "Ram",
                    "message": {
                        "max": msg_max,
                        "count": msg_count,
                        "topic_nodes": topic_nodes,
                        "topic_values": topic_values,
                    },
                })
            }
            #[cfg(any(feature = "sled", feature = "redis"))]
            Retainer::Storage(r) => {
                let msg_max = r.max().await;
                let msg_count = r.count().await;
                let msg_queue_count = r.msg_queue_count.load(Ordering::Relaxed);
                let storage_info = r.storage_db.info().await.unwrap_or_default();
                json!({
                    "storage_info": storage_info,
                    "msg_queue_count": msg_queue_count,
                    "message": {
                        "max": msg_max,
                        "count": msg_count,
                    },
                })
            }
        }
    }
}

pub(crate) const ERR_NOT_SUPPORTED: &str =
    "The storage engine of the 'rmqtt-retainer' plugin does not support cluster mode!";
