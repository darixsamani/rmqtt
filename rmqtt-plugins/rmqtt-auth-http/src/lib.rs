#![deny(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use bytestring::ByteString;
use reqwest::{
    header::{HeaderMap, CONTENT_TYPE},
    Method, Response, Url,
};
use serde::ser::Serialize;
use tokio::sync::RwLock;

use rmqtt::{
    acl::{AuthInfo, Rule},
    codec::v5::SubscribeAckReason,
    context::ServerContext,
    hook::{Handler, HookResult, Parameter, Register, ReturnType, Type},
    macros::Plugin,
    plugin::{PackageInfo, Plugin},
    register,
    types::{
        AuthResult, ConnectInfo, DashMap, Disconnect, Id, Message, Password, PublishAclResult, Reason,
        SubscribeAclResult, Superuser, TimestampMillis, TopicName,
    },
    utils::timestamp_millis,
    Error, Result,
};

use config::PluginConfig;

mod config;

type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;

const CACHEABLE: &str = "X-Cache";
const SUPERUSER: &str = "X-Superuser";
// const CACHE_KEY: &str = "ACL-CACHE-MAP";

#[derive(Clone, Debug)]
struct ResponseResult {
    permission: Permission,
    superuser: Superuser,
    cacheable: Cacheable,
    expire_at: Option<Duration>,
    acl_data: Option<serde_json::Value>,
}

impl ResponseResult {
    #[inline]
    fn new(permission: Permission, superuser: Superuser, cacheable: Cacheable) -> ResponseResult {
        ResponseResult { permission, superuser, cacheable, expire_at: None, acl_data: None }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Permission {
    Allow(Superuser),
    Deny,
    Ignore,
}

impl TryFrom<(&str, Superuser)> for Permission {
    type Error = Error;

    #[inline]
    fn try_from((s, superuser): (&str, Superuser)) -> std::result::Result<Self, Self::Error> {
        match s {
            "allow" => Ok(Permission::Allow(superuser)),
            "deny" => Ok(Permission::Deny),
            "ignore" => Ok(Permission::Ignore),
            _ => Err(anyhow!(
                "The authentication result is incorrect; only 'allow,' 'deny,' or 'ignore' are permitted.",
            )),
        }
    }
}

impl Permission {
    #[inline]
    fn from(s: &str, superuser: Superuser) -> Self {
        match s {
            "allow" => Permission::Allow(superuser),
            "deny" => Permission::Deny,
            "ignore" => Permission::Ignore,
            _ => Permission::Allow(superuser),
        }
    }
}

type Cacheable = Option<i64>;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Copy)]
enum ACLType {
    Sub = 1,
    Pub = 2,
}

impl ACLType {
    fn as_str(&self) -> &str {
        match self {
            Self::Sub => "1",
            Self::Pub => "2",
        }
    }
}

register!(AuthHttpPlugin::new);

type Caches = Arc<DashMap<Id, std::collections::BTreeMap<TopicName, (Permission, TimestampMillis)>>>;

#[derive(Plugin)]
struct AuthHttpPlugin {
    scx: ServerContext,
    httpc: reqwest::Client,
    register: Box<dyn Register>,
    cfg: Arc<RwLock<PluginConfig>>,
    caches: Caches,
}

impl AuthHttpPlugin {
    #[inline]
    async fn new<S: Into<String>>(scx: ServerContext, name: S) -> Result<Self> {
        let name = name.into();
        let cfg = Arc::new(RwLock::new(scx.plugins.read_config::<PluginConfig>(&name)?));
        log::debug!("{} AuthHttpPlugin cfg: {:?}", name, cfg.read().await);
        let register = scx.extends.hook_mgr().register();
        let caches = Arc::new(DashMap::default());
        let httpc = new_reqwest_client()?;
        Ok(Self { scx, httpc, register, cfg, caches })
    }
}

#[async_trait]
impl Plugin for AuthHttpPlugin {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        log::info!("{} init", self.name());
        let cfg = &self.cfg;

        let priority = cfg.read().await.priority;
        self.register
            .add_priority(
                Type::ClientAuthenticate,
                priority,
                Box::new(AuthHandler::new(&self.scx, self.httpc.clone(), cfg, &self.caches)),
            )
            .await;
        self.register
            .add_priority(
                Type::ClientSubscribeCheckAcl,
                priority,
                Box::new(AuthHandler::new(&self.scx, self.httpc.clone(), cfg, &self.caches)),
            )
            .await;
        self.register
            .add_priority(
                Type::MessagePublishCheckAcl,
                priority,
                Box::new(AuthHandler::new(&self.scx, self.httpc.clone(), cfg, &self.caches)),
            )
            .await;
        self.register
            .add(
                Type::ClientKeepalive,
                Box::new(AuthHandler::new(&self.scx, self.httpc.clone(), cfg, &self.caches)),
            )
            .await;

        self.register
            .add(
                Type::ClientDisconnected,
                Box::new(AuthHandler::new(&self.scx, self.httpc.clone(), cfg, &self.caches)),
            )
            .await;
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
        self.register.start().await;
        Ok(())
    }

    #[inline]
    async fn stop(&mut self) -> Result<bool> {
        log::info!("{} stop", self.name());
        self.register.stop().await;
        Ok(true)
    }

    #[inline]
    async fn attrs(&self) -> serde_json::Value {
        let mut stats = HashMap::default();
        for (i, c) in self.caches.iter().enumerate() {
            if i < 1000 {
                stats.insert(c.key().to_string(), c.value().len());
            }
        }

        serde_json::json!({
            "caches": self.caches.len(),
            "stats": stats,
        })
    }
}

struct AuthHandler {
    scx: ServerContext,
    httpc: reqwest::Client,
    cfg: Arc<RwLock<PluginConfig>>,
    caches: Caches,
}

impl AuthHandler {
    fn new(
        scx: &ServerContext,
        httpc: reqwest::Client,
        cfg: &Arc<RwLock<PluginConfig>>,
        caches: &Caches,
    ) -> Self {
        Self { scx: scx.clone(), httpc, cfg: cfg.clone(), caches: caches.clone() }
    }

    async fn response_result(resp: Response) -> Result<ResponseResult> {
        if resp.status().is_success() {
            let content_type = resp.headers().get(CONTENT_TYPE);
            let is_json_content_type =
                content_type.map(|hv| hv.as_bytes().starts_with(b"application/json")).unwrap_or_default();
            log::debug!("content_type: {content_type:?}");
            log::debug!("is_json_content_type: {is_json_content_type}");
            let superuser = resp.headers().contains_key(SUPERUSER);
            // let acl = resp.headers().contains_key(ACL);
            let cache_timeout = if let Some(tm) = resp.headers().get(CACHEABLE).and_then(|v| v.to_str().ok())
            {
                match tm.parse::<i64>() {
                    Ok(tm) => Some(tm),
                    Err(e) => {
                        log::warn!("Parse X-Cache error, {e:?}");
                        None
                    }
                }
            } else {
                None
            };
            log::debug!("Cache timeout is {cache_timeout:?}");
            let resp = if is_json_content_type {
                let mut body: serde_json::Value = resp.json().await?;
                log::debug!("body: {body:?}");
                if let Some(obj) = body.as_object_mut() {
                    let result = obj
                        .get("result")
                        .and_then(|res| res.as_str())
                        .ok_or_else(|| anyhow!("Authentication result does not exist"))?;
                    let superuser = obj.get("superuser").and_then(|res| res.as_bool()).unwrap_or(superuser);
                    let expire_at =
                        obj.get("expire_at").and_then(|res| res.as_u64().map(Duration::from_secs));
                    let permission = Permission::try_from((result, superuser))?;
                    let acl_data = obj.remove("acl");

                    ResponseResult { permission, superuser, cacheable: cache_timeout, expire_at, acl_data }
                } else if let Some(body) = body.as_str() {
                    log::debug!("body: {body:?}");
                    ResponseResult::new(Permission::try_from((body, superuser))?, superuser, cache_timeout)
                } else {
                    return Err(anyhow!(format!("The response result is incorrect, {}", body)));
                }
            } else {
                let body = resp.text().await?;
                log::debug!("body: {body:?}");
                ResponseResult::new(Permission::from(body.as_str(), superuser), superuser, cache_timeout)
            };
            Ok(resp)
        } else {
            Ok(ResponseResult::new(Permission::Ignore, false, None))
        }
    }

    async fn http_get_request<T: Serialize + ?Sized>(
        httpc: &reqwest::Client,
        url: Url,
        body: &T,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ResponseResult> {
        log::debug!("http_get_request, timeout: {timeout:?}, url: {url}");
        match httpc.get(url).headers(headers).timeout(timeout).query(body).send().await {
            Err(e) => {
                log::warn!("{e:?}");
                Err(anyhow!(e))
            }
            Ok(resp) => Self::response_result(resp).await,
        }
    }

    async fn http_form_request<T: Serialize + ?Sized>(
        httpc: &reqwest::Client,
        url: Url,
        method: Method,
        body: &T,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ResponseResult> {
        log::debug!("http_form_request, method: {method:?}, timeout: {timeout:?}, url: {url}");
        match httpc.request(method, url).headers(headers).timeout(timeout).form(body).send().await {
            Err(e) => {
                log::warn!("{e:?}");
                Err(anyhow!(e))
            }
            Ok(resp) => Self::response_result(resp).await,
        }
    }

    async fn http_json_request<T: Serialize + ?Sized>(
        httpc: &reqwest::Client,
        url: Url,
        method: Method,
        body: &T,
        headers: HeaderMap,
        timeout: Duration,
    ) -> Result<ResponseResult> {
        log::debug!("http_json_request, method: {method:?}, timeout: {timeout:?}, url: {url}");
        match httpc.request(method, url).headers(headers).timeout(timeout).json(body).send().await {
            Err(e) => {
                log::warn!("{e:?}");
                Err(anyhow!(e))
            }
            Ok(resp) => Self::response_result(resp).await,
        }
    }

    fn replaces(
        params: &mut HashMap<String, String>,
        id: &Id,
        password: Option<&Password>,
        protocol: Option<u8>,
        sub_or_pub: Option<(ACLType, &TopicName)>,
    ) -> Result<()> {
        let password =
            if let Some(p) = password { ByteString::try_from(p.clone())? } else { ByteString::default() };
        let client_id = id.client_id.as_ref();
        let username = id.username.as_ref().map(|n| n.as_ref()).unwrap_or("");
        let remote_addr = id.remote_addr.map(|addr| addr.ip().to_string()).unwrap_or_default();
        for v in params.values_mut() {
            *v = v.replace("%u", username);
            *v = v.replace("%c", client_id);
            *v = v.replace("%a", &remote_addr);
            *v = v.replace("%P", &password);
            if let Some(protocol) = protocol {
                let mut buffer = itoa::Buffer::new();
                *v = v.replace("%r", buffer.format(protocol));
            }
            if let Some((ref acl_type, topic)) = sub_or_pub {
                *v = v.replace("%A", acl_type.as_str());
                *v = v.replace("%t", topic);
            } else {
                *v = v.replace("%A", "");
                *v = v.replace("%t", "");
            }
        }
        Ok(())
    }

    async fn request(
        &self,
        id: &Id,
        mut req_cfg: config::Req,
        password: Option<&Password>,
        protocol: Option<u8>,
        sub_or_pub: Option<(ACLType, &TopicName)>,
    ) -> Result<ResponseResult> {
        log::debug!("{:?} req_cfg.url.path(): {:?}", id, req_cfg.url.path());
        let (headers, timeout) = {
            let cfg = self.cfg.read().await;
            let headers = match (cfg.headers(), req_cfg.headers()) {
                (Some(def_headers), Some(req_headers)) => {
                    let mut headers = def_headers.clone();
                    headers.extend(req_headers.clone());
                    headers
                }
                (Some(def_headers), None) => def_headers.clone(),
                (None, Some(req_headers)) => req_headers.clone(),
                (None, None) => HeaderMap::new(),
            };
            (headers, cfg.http_timeout)
        };

        let auth_result = if req_cfg.is_get() {
            let body = &mut req_cfg.params;
            Self::replaces(body, id, password, protocol, sub_or_pub)?;
            Self::http_get_request(&self.httpc, req_cfg.url, body, headers, timeout).await?
        } else if req_cfg.json_body() {
            let body = &mut req_cfg.params;
            Self::replaces(body, id, password, protocol, sub_or_pub)?;
            Self::http_json_request(&self.httpc, req_cfg.url, req_cfg.method, body, headers, timeout).await?
        } else {
            //form body
            let body = &mut req_cfg.params;
            Self::replaces(body, id, password, protocol, sub_or_pub)?;
            Self::http_form_request(&self.httpc, req_cfg.url, req_cfg.method, body, headers, timeout).await?
        };
        log::debug!("auth_result: {auth_result:?}");
        Ok(auth_result)
    }

    #[inline]
    async fn auth(&self, connect_info: &ConnectInfo) -> (Permission, Option<AuthInfo>) {
        if let Some(req) = { self.cfg.read().await.http_auth_req.clone() } {
            match self
                .request(
                    connect_info.id(),
                    req,
                    connect_info.password(),
                    Some(connect_info.proto_ver()),
                    None,
                )
                .await
            {
                Ok(auth_res) => {
                    log::debug!("auth result: {auth_res:?}");
                    let auth_info = if matches!(auth_res.permission, Permission::Allow(_)) {
                        if let Some(acl_data) =
                            auth_res.acl_data.as_ref().and_then(|acl_data| acl_data.as_array())
                        {
                            match acl_data
                                .iter()
                                .map(|acl| Rule::try_from((acl, connect_info)))
                                .collect::<Result<Vec<Rule>>>()
                            {
                                Ok(rules) => {
                                    let auth_info = AuthInfo {
                                        superuser: auth_res.superuser,
                                        expire_at: auth_res.expire_at,
                                        rules,
                                    };
                                    log::debug!("auth_info: {auth_info:?}");
                                    Some(auth_info)
                                }
                                Err(e) => {
                                    log::warn!("{} {}", connect_info.id(), e);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    (auth_res.permission, auth_info)
                }
                Err(e) => {
                    log::warn!("{:?} auth error, {:?}", connect_info.id(), e);
                    if self.cfg.read().await.deny_if_error {
                        (Permission::Deny, None)
                    } else {
                        (Permission::Ignore, None)
                    }
                }
            }
        } else {
            (Permission::Ignore, None)
        }
    }

    #[inline]
    async fn acl(
        &self,
        id: &Id,
        protocol: Option<u8>,
        sub_or_pub: Option<(ACLType, &TopicName)>,
    ) -> (Permission, Cacheable) {
        if let Some(req) = { self.cfg.read().await.http_acl_req.clone() } {
            match self.request(id, req, None, protocol, sub_or_pub).await {
                Ok(acl_res) => {
                    log::debug!("acl result: {acl_res:?}");
                    (acl_res.permission, acl_res.cacheable)
                }
                Err(e) => {
                    log::warn!("{id:?} acl error, {e:?}");
                    if self.cfg.read().await.deny_if_error {
                        (Permission::Deny, None)
                    } else {
                        (Permission::Ignore, None)
                    }
                }
            }
        } else {
            (Permission::Ignore, None)
        }
    }

    #[inline]
    fn cache_set(&self, id: Id, topic: TopicName, perm: Permission, expire: TimestampMillis) {
        self.caches.entry(id).or_default().insert(topic, (perm, expire));
    }

    #[inline]
    fn cache_get(&self, id: &Id, topic: &TopicName) -> Option<(Permission, TimestampMillis)> {
        self.caches.get(id).and_then(|c| c.get(topic).map(|(perm, expire)| (*perm, *expire)))
    }

    #[inline]
    fn cache_remove(&self, id: &Id) {
        self.caches.remove(id);
    }
}

#[async_trait]
impl Handler for AuthHandler {
    async fn hook(&self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        match param {
            Parameter::ClientAuthenticate(connect_info) => {
                log::debug!("ClientAuthenticate auth-http");
                if matches!(
                    acc,
                    Some(HookResult::AuthResult(AuthResult::BadUsernameOrPassword))
                        | Some(HookResult::AuthResult(AuthResult::NotAuthorized))
                ) {
                    return (false, acc);
                }

                return match self.auth(connect_info).await {
                    (Permission::Allow(superuser), auth_info) => {
                        if auth_info.as_ref().map(|ai| ai.is_expired()).unwrap_or_default() {
                            log::warn!("{} authentication information has expired.", connect_info.id());
                            (false, Some(HookResult::AuthResult(AuthResult::NotAuthorized)))
                        } else {
                            (false, Some(HookResult::AuthResult(AuthResult::Allow(superuser, auth_info))))
                        }
                    }
                    (Permission::Deny, _) => {
                        (false, Some(HookResult::AuthResult(AuthResult::BadUsernameOrPassword)))
                    }
                    (Permission::Ignore, _) => (true, None),
                };
            }

            Parameter::ClientSubscribeCheckAcl(session, subscribe) => {
                if let Some(HookResult::SubscribeAclResult(acl_result)) = &acc {
                    if acl_result.failure() {
                        return (false, acc);
                    }
                }

                if let Some(auth_info) = &session.auth_info {
                    if let Some(acl_res) = auth_info.subscribe_acl(subscribe).await {
                        return acl_res;
                    }
                }

                //Permission, Cacheable
                let (acl_res, _) = self
                    .acl(
                        &session.id,
                        session.protocol().await.ok(),
                        Some((ACLType::Sub, &subscribe.topic_filter)),
                    )
                    .await;
                return match acl_res {
                    Permission::Allow(_) => (
                        false,
                        Some(HookResult::SubscribeAclResult(SubscribeAclResult::new_success(
                            subscribe.opts.qos(),
                            None,
                        ))),
                    ),
                    Permission::Deny => (
                        false,
                        Some(HookResult::SubscribeAclResult(SubscribeAclResult::new_failure(
                            SubscribeAckReason::NotAuthorized,
                        ))),
                    ),
                    Permission::Ignore => (true, None),
                };
            }

            Parameter::MessagePublishCheckAcl(session, publish) => {
                log::debug!("MessagePublishCheckAcl");
                if let Some(HookResult::PublishAclResult(PublishAclResult::Rejected(_))) = &acc {
                    return (false, acc);
                }

                if let Some(auth_info) = &session.auth_info {
                    if let Some(acl_res) =
                        auth_info.publish_acl(publish, self.cfg.read().await.disconnect_if_pub_rejected).await
                    {
                        return acl_res;
                    }
                }

                let acl_res = if let Some((acl_res, expire)) = self.cache_get(&session.id, &publish.topic) {
                    if expire < 0 || timestamp_millis() < expire {
                        Some(acl_res)
                    } else {
                        None
                    }
                } else {
                    None
                };

                let acl_res = if let Some(acl_res) = acl_res {
                    acl_res
                } else {
                    //Permission, Cacheable
                    let (acl_res, cacheable) = self
                        .acl(&session.id, session.protocol().await.ok(), Some((ACLType::Pub, &publish.topic)))
                        .await;
                    if let Some(tm) = cacheable {
                        let expire = if tm < 0 { tm } else { timestamp_millis() + tm };

                        self.cache_set(session.id.clone(), publish.topic.clone(), acl_res, expire);
                    }
                    acl_res
                };

                return match acl_res {
                    Permission::Allow(_) => {
                        (false, Some(HookResult::PublishAclResult(PublishAclResult::Allow)))
                    }
                    Permission::Deny => (
                        false,
                        Some(HookResult::PublishAclResult(PublishAclResult::Rejected(
                            self.cfg.read().await.disconnect_if_pub_rejected,
                        ))),
                    ),
                    Permission::Ignore => (true, None),
                };
            }

            Parameter::ClientKeepalive(s, _) => {
                if let Some(auth) = &s.auth_info {
                    log::debug!("Keepalive auth-http, is_expired: {:?}", auth.is_expired());
                    if auth.is_expired() && self.cfg.read().await.disconnect_if_expiry {
                        if let Some(tx) = self.scx.extends.shared().await.entry(s.id().clone()).tx() {
                            if let Err(e) = tx.unbounded_send(Message::Closed(Reason::ConnectDisconnect(
                                Some(Disconnect::Other("Http Auth expired".into())),
                            ))) {
                                log::warn!("{} {}", s.id(), e);
                            }
                        }
                    }
                }
            }

            Parameter::ClientDisconnected(s, _) => {
                self.cache_remove(&s.id);
            }

            _ => {
                log::error!("unimplemented, {param:?}")
            }
        }
        (true, acc)
    }
}

fn new_reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow!(e))
}
