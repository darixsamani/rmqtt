//! MQTT Broker Plugin Management System
//!
//! Provides a robust plugin architecture with:
//! - Dynamic loading/unloading
//! - Lifecycle management
//! - Configuration handling
//! - Inter-plugin communication
//!
//! ## Core Functionality
//! 1. ​**​Plugin Lifecycle​**​:
//!    - Registration and initialization
//!    - Startup/shutdown sequencing
//!    - Immutable plugin support
//!    - State tracking (active/inactive)
//!
//! 2. ​**​Configuration Management​**​:
//!    - File-based configuration
//!    - Environment variable overrides
//!    - Default value handling
//!    - Runtime reload capability
//!
//! 3. ​**​Plugin Operations​**​:
//!    - Metadata inspection
//!    - Message passing
//!    - Thread-safe access
//!    - Dependency management
//!
//! ## Key Features
//! - Async-friendly interface
//! - Atomic state transitions
//! - Flexible configuration system
//! - Plugin isolation
//! - Comprehensive metadata
//!
//! ## Implementation Details
//! - DashMap for concurrent storage
//! - Async trait patterns
//! - Type-erased plugin instances
//! - JSON-based configuration
//! - Environment-aware config loading
//!
//! Usage Patterns:
//! 1. Implement `Plugin` trait for custom functionality
//! 2. Register with `register!` macro
//! 3. Manage via `Manager` interface:
//!    - `start()`/`stop()`
//!    - `load_config()`
//!    - `send()` messages
//! 4. Query plugin info/metadata
//!
//! Note: Plugins can be marked immutable to prevent
//! runtime modifications for critical components.

use std::future::Future;
use std::pin::Pin;

use anyhow::anyhow;
use async_trait::async_trait;
use config::FileFormat::Toml;
use config::{Config, File, Source};
use dashmap::iter::Iter;
use dashmap::mapref::one::{Ref, RefMut};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::types::DashMap;
use crate::Result;

pub type EntryRef<'a> = Ref<'a, String, Entry>;
pub type EntryRefMut<'a> = RefMut<'a, String, Entry>;
pub type EntryIter<'a> = Iter<'a, String, Entry, ahash::RandomState, DashMap<String, Entry>>;

#[macro_export]
macro_rules! register {
    ($name:path) => {
        #[inline]
        pub async fn register_named(
            scx: &rmqtt::context::ServerContext,
            name: &'static str,
            default_startup: bool,
            immutable: bool,
        ) -> rmqtt::Result<()> {
            let scx1 = scx.clone();
            scx.plugins
                .register(name, default_startup, immutable, move || -> rmqtt::plugin::DynPluginResult {
                    let scx1 = scx1.clone();
                    Box::pin(async move {
                        $name(scx1.clone(), name).await.map(|p| -> rmqtt::plugin::DynPlugin { Box::new(p) })
                    })
                })
                .await?;
            Ok(())
        }

        #[inline]
        pub async fn register(
            scx: &rmqtt::context::ServerContext,
            default_startup: bool,
            immutable: bool,
        ) -> rmqtt::Result<()> {
            let name = env!("CARGO_PKG_NAME");
            register_named(scx, env!("CARGO_PKG_NAME"), default_startup, immutable).await
        }
    };
}

#[async_trait]
pub trait Plugin: PackageInfo + Send + Sync {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        Ok(())
    }

    #[inline]
    async fn get_config(&self) -> Result<serde_json::Value> {
        Ok(json!({}))
    }

    #[inline]
    async fn load_config(&mut self) -> Result<()> {
        Err(anyhow!("unimplemented!"))
    }

    #[inline]
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }

    #[inline]
    async fn stop(&mut self) -> Result<bool> {
        Ok(true)
    }

    #[inline]
    async fn attrs(&self) -> serde_json::Value {
        serde_json::Value::Null
    }

    #[inline]
    async fn send(&self, _msg: serde_json::Value) -> Result<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

pub trait PackageInfo {
    fn name(&self) -> &str;

    #[inline]
    fn version(&self) -> &str {
        "0.0.0"
    }

    #[inline]
    fn descr(&self) -> Option<&str> {
        None
    }

    #[inline]
    fn authors(&self) -> Option<Vec<&str>> {
        None
    }

    #[inline]
    fn homepage(&self) -> Option<&str> {
        None
    }

    #[inline]
    fn license(&self) -> Option<&str> {
        None
    }

    #[inline]
    fn repository(&self) -> Option<&str> {
        None
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
// type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

pub trait PluginFn: 'static + Sync + Send + Fn() -> BoxFuture<Result<DynPlugin>> {}

impl<T> PluginFn for T where T: 'static + Sync + Send + ?Sized + Fn() -> BoxFuture<Result<DynPlugin>> {}

pub type DynPluginResult = BoxFuture<Result<DynPlugin>>;
pub type DynPlugin = Box<dyn Plugin>;
pub type DynPluginFn = Box<dyn PluginFn>;

pub struct Entry {
    inited: bool,
    active: bool,
    //will reject start, stop, and load config operations
    immutable: bool,
    plugin: Option<DynPlugin>,
    plugin_f: Option<DynPluginFn>,
}

impl Entry {
    #[inline]
    pub fn inited(&self) -> bool {
        self.inited
    }

    #[inline]
    pub fn active(&self) -> bool {
        self.active
    }

    #[inline]
    pub fn immutable(&self) -> bool {
        self.immutable
    }

    #[inline]
    async fn plugin(&self) -> Result<&dyn Plugin> {
        if let Some(plugin) = &self.plugin {
            Ok(plugin.as_ref())
        } else {
            Err(anyhow!("the plug-in is not initialized"))
        }
    }

    #[inline]
    async fn plugin_mut(&mut self) -> Result<&mut dyn Plugin> {
        if let Some(plugin_f) = self.plugin_f.take() {
            self.plugin.replace(plugin_f().await?);
        }

        if let Some(plugin) = self.plugin.as_mut() {
            Ok(plugin.as_mut())
        } else {
            Err(anyhow!("the plug-in is not initialized"))
        }
    }

    #[inline]
    pub async fn to_info(&self, name: &str) -> Result<PluginInfo> {
        if let Ok(plugin) = self.plugin().await {
            let attrs = serde_json::to_vec(&plugin.attrs().await)?;
            Ok(PluginInfo {
                name: plugin.name().to_owned(),
                version: Some(plugin.version().to_owned()),
                descr: plugin.descr().map(String::from),
                authors: plugin.authors().map(|authors| authors.into_iter().map(String::from).collect()),
                homepage: plugin.homepage().map(String::from),
                license: plugin.license().map(String::from),
                repository: plugin.repository().map(String::from),

                inited: self.inited,
                active: self.active,
                immutable: self.immutable,
                attrs,
            })
        } else {
            Ok(PluginInfo {
                name: name.to_owned(),
                inited: self.inited,
                active: self.active,
                immutable: self.immutable,
                ..Default::default()
            })
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
pub struct PluginInfo {
    pub name: String,
    pub version: Option<String>,
    pub descr: Option<String>,
    pub authors: Option<Vec<String>>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub repository: Option<String>,

    pub inited: bool,
    pub active: bool,
    pub immutable: bool,
    pub attrs: Vec<u8>, //json data
}

impl PluginInfo {
    #[inline]
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let attrs = if self.attrs.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&self.attrs)?
        };
        Ok(json!({
            "name": self.name,
            "version": self.version,
            "descr": self.descr,
            "authors": self.authors,
            "homepage": self.homepage,
            "license": self.license,
            "repository": self.repository,

            "inited": self.inited,
            "active": self.active,
            "immutable": self.immutable,
            "attrs": attrs,
        }))
    }
}
pub enum PluginManagerConfig {
    Path(String),
    Map(crate::types::HashMap<String, String>),
}
pub struct Manager {
    plugins: DashMap<String, Entry>,
    config: PluginManagerConfig,
}

impl Manager {
    pub(crate) fn new(config: PluginManagerConfig) -> Self {
        Self { plugins: DashMap::default(), config }
    }

    ///Register a Plugin
    pub async fn register<N: Into<String>, F: PluginFn>(
        &self,
        name: N,
        default_startup: bool,
        immutable: bool,
        plugin_f: F,
    ) -> Result<()> {
        let name = name.into();

        if let Some((_, mut entry)) = self.plugins.remove(&name) {
            if entry.active {
                entry.plugin_mut().await?.stop().await?;
            }
        }

        let (plugin, plugin_f) = if default_startup {
            let mut plugin = plugin_f().await?;
            plugin.init().await?;
            plugin.start().await?;
            (Some(plugin), None)
        } else {
            let boxed_f: Box<dyn PluginFn> = Box::new(plugin_f);
            (None, Some(boxed_f))
        };

        let entry = Entry { inited: default_startup, active: default_startup, immutable, plugin, plugin_f };
        self.plugins.insert(name, entry);
        Ok(())
    }

    ///Return Config
    pub async fn get_config(&self, name: &str) -> Result<serde_json::Value> {
        if let Some(entry) = self.get(name) {
            entry.plugin().await?.get_config().await
        } else {
            Err(anyhow!(format!("{} the plug-in does not exist", name)))
        }
    }

    ///Load Config
    pub async fn load_config(&self, name: &str) -> Result<()> {
        if let Some(mut entry) = self.get_mut(name)? {
            if entry.inited {
                entry.plugin_mut().await?.load_config().await?;
                Ok(())
            } else {
                Err(anyhow!("the plug-in is not initialized"))
            }
        } else {
            Err(anyhow!(format!("{} the plug-in does not exist", name)))
        }
    }

    ///Start a Plugin
    pub async fn start(&self, name: &str) -> Result<()> {
        if let Some(mut entry) = self.get_mut(name)? {
            if !entry.inited {
                entry.plugin_mut().await?.init().await?;
                entry.inited = true;
            }
            if !entry.active {
                entry.plugin_mut().await?.start().await?;
                entry.active = true;
            }
            Ok(())
        } else {
            Err(anyhow!(format!("{} the plug-in does not exist", name)))
        }
    }

    ///Stop a Plugin
    pub async fn stop(&self, name: &str) -> Result<bool> {
        if let Some(mut entry) = self.get_mut(name)? {
            if entry.active {
                let stopped = entry.plugin_mut().await?.stop().await?;
                entry.active = !stopped;
                Ok(stopped)
            } else {
                Err(anyhow!(format!("{} the plug-in is not started", name)))
            }
        } else {
            Err(anyhow!(format!("{} the plug-in does not exist", name)))
        }
    }

    ///Plugin is active
    pub fn is_active(&self, name: &str) -> bool {
        if let Some(entry) = self.plugins.get(name) {
            entry.active()
        } else {
            false
        }
    }

    ///Get a Plugin
    pub fn get(&self, name: &str) -> Option<EntryRef<'_>> {
        self.plugins.get(name)
    }

    ///Get a mut Plugin
    pub fn get_mut(&self, name: &str) -> Result<Option<EntryRefMut<'_>>> {
        if let Some(entry) = self.plugins.get_mut(name) {
            if entry.immutable {
                Err(anyhow!("the plug-in is immutable"))
            } else {
                Ok(Some(entry))
            }
        } else {
            Ok(None)
        }
    }

    ///Sending messages to plug-in
    pub async fn send(&self, name: &str, msg: serde_json::Value) -> Result<serde_json::Value> {
        if let Some(entry) = self.plugins.get(name) {
            entry.plugin().await?.send(msg).await
        } else {
            Err(anyhow!(format!("{} the plug-in does not exist", name)))
        }
    }

    ///List Plugins
    pub fn iter(&self) -> EntryIter<'_> {
        self.plugins.iter()
    }

    ///Read plugin Config
    pub fn read_config<'de, T: serde::Deserialize<'de>>(&self, name: &str) -> Result<T> {
        let (cfg, _) = self.read_config_with_required(name, true, &[])?;
        Ok(cfg)
    }

    pub fn read_config_default<'de, T: serde::Deserialize<'de>>(&self, name: &str) -> Result<T> {
        let (cfg, def) = self.read_config_with_required(name, false, &[])?;
        if def {
            log::warn!("The configuration for plugin '{name}' does not exist, default values will be used!");
        }
        Ok(cfg)
    }

    pub fn read_config_with<'de, T: serde::Deserialize<'de>>(
        &self,
        name: &str,
        env_list_keys: &[&str],
    ) -> Result<T> {
        let (cfg, _) = self.read_config_with_required(name, true, env_list_keys)?;
        Ok(cfg)
    }

    pub fn read_config_default_with<'de, T: serde::Deserialize<'de>>(
        &self,
        name: &str,
        env_list_keys: &[&str],
    ) -> Result<T> {
        let (cfg, def) = self.read_config_with_required(name, false, env_list_keys)?;
        if def {
            log::warn!("The configuration for plugin '{name}' does not exist, default values will be used!");
        }
        Ok(cfg)
    }

    pub fn read_config_with_required<'de, T: serde::Deserialize<'de>>(
        &self,
        name: &str,
        required: bool,
        env_list_keys: &[&str],
    ) -> Result<(T, bool)> {
        let mut builder = match self.config {
            PluginManagerConfig::Path(ref path) => {
                let path = path.trim_end_matches(['/', '\\']);
                Config::builder().add_source(File::with_name(&format!("{path}/{name}")).required(required))
            }
            PluginManagerConfig::Map(ref map) => {
                let default_config = "".to_owned();
                let config_string = map.get(name).unwrap_or(&default_config);
                Config::builder().add_source(File::from_str(config_string, Toml).required(required))
            }
        };

        let mut env = config::Environment::with_prefix(&format!("rmqtt_plugin_{}", name.replace('-', "_")));
        if !env_list_keys.is_empty() {
            env = env.try_parsing(true).list_separator(" ");
            for key in env_list_keys {
                env = env.with_list_parse_key(key);
            }
        }
        builder = builder.add_source(env);

        let s = builder.build()?;
        let count = s.collect()?.len();
        Ok((s.try_deserialize::<T>()?, count == 0))
    }
}
