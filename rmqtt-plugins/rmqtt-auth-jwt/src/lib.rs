#![deny(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use itoa::Buffer;
use jsonwebtoken::{decode, TokenData, Validation};
use tokio::sync::RwLock;

use rmqtt::{
    acl::{
        AuthInfo, Rule, PLACEHOLDER_CLIENTID, PLACEHOLDER_IPADDR, PLACEHOLDER_PROTOCOL, PLACEHOLDER_USERNAME,
    },
    context::ServerContext,
    hook::{Handler, HookResult, Parameter, Register, ReturnType, Type},
    macros::Plugin,
    plugin::{PackageInfo, Plugin},
    register,
    types::{AuthResult, ConnectInfo, Disconnect, Message, PublishAclResult, Reason},
    Result,
};

use config::{JWTFrom, PluginConfig, ValidateClaims};

mod config;

type HashMap<K, V> = std::collections::HashMap<K, V, ahash::RandomState>;

register!(AuthJwtPlugin::new);

#[derive(Plugin)]
struct AuthJwtPlugin {
    scx: ServerContext,
    register: Box<dyn Register>,
    cfg: Arc<RwLock<PluginConfig>>,
}

impl AuthJwtPlugin {
    #[inline]
    async fn new<S: Into<String>>(scx: ServerContext, name: S) -> Result<Self> {
        let name = name.into();
        let mut cfg = scx.plugins.read_config::<PluginConfig>(&name)?;
        cfg.init_decoding_key()?;
        let cfg = Arc::new(RwLock::new(cfg));
        log::info!("{} AuthJwtPlugin cfg: {:?}", name, cfg.read().await);
        let register = scx.extends.hook_mgr().register();
        Ok(Self { scx, register, cfg })
    }
}

#[async_trait]
impl Plugin for AuthJwtPlugin {
    #[inline]
    async fn init(&mut self) -> Result<()> {
        log::info!("{} init", self.name());
        let cfg = &self.cfg;

        let priority = cfg.read().await.priority;
        self.register
            .add_priority(Type::ClientAuthenticate, priority, Box::new(AuthHandler::new(&self.scx, cfg)))
            .await;
        self.register
            .add_priority(Type::ClientSubscribeCheckAcl, priority, Box::new(AuthHandler::new(&self.scx, cfg)))
            .await;
        self.register
            .add_priority(Type::MessagePublishCheckAcl, priority, Box::new(AuthHandler::new(&self.scx, cfg)))
            .await;
        self.register.add(Type::ClientKeepalive, Box::new(AuthHandler::new(&self.scx, cfg))).await;
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
        serde_json::json!({})
    }
}

struct AuthHandler {
    scx: ServerContext,
    cfg: Arc<RwLock<PluginConfig>>,
}

impl AuthHandler {
    fn new(scx: &ServerContext, cfg: &Arc<RwLock<PluginConfig>>) -> Self {
        Self { scx: scx.clone(), cfg: cfg.clone() }
    }

    #[inline]
    async fn token<'a>(&self, connect_info: &'a ConnectInfo) -> Option<Cow<'a, str>> {
        let token = match self.cfg.read().await.from {
            JWTFrom::Username => connect_info.username().map(|u| Cow::Borrowed(u.as_ref())),
            JWTFrom::Password => connect_info.password().map(|p| String::from_utf8_lossy(p)),
        };
        token
    }

    #[inline]
    fn replaces(
        connect_info: &ConnectInfo,
        item: &str,
        p_uname: bool,
        p_cid: bool,
        p_ipaddr: bool,
        p_proto: bool,
    ) -> Result<String> {
        let mut item = if p_uname {
            if let Some(username) = connect_info.username() {
                Cow::Owned(item.replace(PLACEHOLDER_USERNAME, username))
            } else {
                return Err(anyhow!("username does not exist"));
            }
        } else {
            Cow::Borrowed(item)
        };
        if p_cid {
            item = Cow::Owned(item.replace(PLACEHOLDER_CLIENTID, connect_info.client_id()));
        }
        if p_ipaddr {
            if let Some(ipaddr) = connect_info.ipaddress() {
                item = Cow::Owned(item.replace(PLACEHOLDER_IPADDR, ipaddr.ip().to_string().as_str()));
            } else {
                return Err(anyhow!("ipaddr does not exist"));
            }
        }
        if p_proto {
            item = Cow::Owned(
                item.replace(PLACEHOLDER_PROTOCOL, Buffer::new().format(connect_info.proto_ver())),
            );
        }
        Ok(item.into())
    }

    #[inline]
    async fn standard_auth(
        &self,
        connect_info: &ConnectInfo,
        token: &str,
        validate_claims_cfg: &ValidateClaims,
    ) -> Result<TokenData<HashMap<String, serde_json::Value>>> {
        let mut required_spec_claims = HashSet::default();

        let validate_exp = validate_claims_cfg.validate_exp_enable;
        let validate_nbf = validate_claims_cfg.validate_nbf_enable;

        let mut validate_aud = false;
        let mut aud = None;
        let mut iss = None;
        let mut sub = None;

        if let Some(validate_aud_cfg) = validate_claims_cfg.validate_aud.as_ref() {
            if !validate_aud_cfg.is_empty() {
                let items = validate_aud_cfg
                    .iter()
                    .map(|(item, p_uname, p_cid, p_ipaddr, p_proto)| {
                        Self::replaces(connect_info, item, *p_uname, *p_cid, *p_ipaddr, *p_proto)
                    })
                    .collect::<Result<HashSet<String>>>()?;
                validate_aud = true;
                aud = Some(items);
                required_spec_claims.insert("aud".into());
            }
        }

        if let Some(validate_iss_cfg) = validate_claims_cfg.validate_iss.as_ref() {
            if !validate_iss_cfg.is_empty() {
                let items = validate_iss_cfg
                    .iter()
                    .map(|(item, p_uname, p_cid, p_ipaddr, p_proto)| {
                        Self::replaces(connect_info, item, *p_uname, *p_cid, *p_ipaddr, *p_proto)
                    })
                    .collect::<Result<HashSet<String>>>()?;
                iss = Some(items);
                required_spec_claims.insert("iss".into());
            }
        }

        if let Some((item, p_uname, p_cid, p_ipaddr, p_proto)) = validate_claims_cfg.validate_sub.as_ref() {
            sub = Some(Self::replaces(connect_info, item, *p_uname, *p_cid, *p_ipaddr, *p_proto)?);
            required_spec_claims.insert("sub".into());
        }

        let header = jsonwebtoken::decode_header(token).map_err(|e| anyhow!(e))?;
        log::debug!("header: {header:?}");
        let mut validation = Validation::new(header.alg);
        validation.validate_exp = validate_exp;
        validation.validate_nbf = validate_nbf;
        validation.validate_aud = validate_aud;
        validation.aud = aud;
        validation.iss = iss;
        validation.sub = sub;
        validation.required_spec_claims = required_spec_claims;

        log::debug!("validation: {validation:?}");

        let token_data = decode::<HashMap<String, serde_json::Value>>(
            token,
            &self.cfg.read().await.decoded_key,
            &validation,
        )
        .map_err(|e| anyhow!(e))?;

        Ok(token_data)
    }

    #[inline]
    fn extended_auth(
        &self,
        connect_info: &ConnectInfo,
        validate_claims_cfg: &ValidateClaims,
        token_data: &TokenData<HashMap<String, serde_json::Value>>,
    ) -> Result<()> {
        let validates = validate_claims_cfg
            .validate_customs
            .iter()
            .map(|(name, items)| {
                items
                    .iter()
                    .map(|(item, p_uname, p_cid, p_ipaddr, p_proto)| {
                        Self::replaces(connect_info, item, *p_uname, *p_cid, *p_ipaddr, *p_proto)
                    })
                    .collect::<Result<Vec<String>>>()
                    .map(|items| (name, items))
            })
            .collect::<Result<Vec<(_, _)>>>()?;

        let failed = validates.into_iter().find_map(|(name, items)| {
            let claim_item = token_data.claims.get(name).and_then(|val| val.as_str());
            let valid_res = claim_item.map(|s| items.iter().any(|item| item == s)).unwrap_or_default();
            if !valid_res {
                Some((name, items, claim_item))
            } else {
                None
            }
        });
        log::debug!("failed: {failed:?}");
        if let Some((name, expecteds, actuals)) = failed {
            Err(anyhow!(format!(
                "{} verification failed, expected value: {:?}, actual value: {:?}",
                name, expecteds, actuals
            )))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl Handler for AuthHandler {
    async fn hook(&self, param: &Parameter, acc: Option<HookResult>) -> ReturnType {
        match param {
            Parameter::ClientAuthenticate(connect_info) => {
                log::debug!("ClientAuthenticate auth-jwt");
                if matches!(
                    acc,
                    Some(HookResult::AuthResult(AuthResult::BadUsernameOrPassword))
                        | Some(HookResult::AuthResult(AuthResult::NotAuthorized))
                ) {
                    return (false, acc);
                }

                let token = match self.token(connect_info).await {
                    Some(token) => token,
                    None => return (false, Some(HookResult::AuthResult(AuthResult::NotAuthorized))),
                };
                log::debug!("ClientAuthenticate token: {token}");

                let validate_claims_cfg = &self.cfg.read().await.validate_claims;
                let token_data =
                    match self.standard_auth(connect_info, token.as_ref(), validate_claims_cfg).await {
                        Ok(token_data) => token_data,
                        Err(e) => {
                            log::warn!("{} {}", connect_info.id(), e);
                            return (false, Some(HookResult::AuthResult(AuthResult::NotAuthorized)));
                        }
                    };

                if let Err(e) = self.extended_auth(connect_info, validate_claims_cfg, &token_data) {
                    log::warn!("{} {}", connect_info.id(), e);
                    return (false, Some(HookResult::AuthResult(AuthResult::NotAuthorized)));
                }

                log::debug!("token_data header: {:?}", token_data.header);
                log::debug!("token_data claims: {:?}", token_data.claims);

                let superuser =
                    token_data.claims.get("superuser").and_then(|v| v.as_bool()).unwrap_or_default();

                let rules = if let Some(acls) = token_data.claims.get("acl").and_then(|acl| acl.as_array()) {
                    match acls
                        .iter()
                        .map(|acl| Rule::try_from((acl, *connect_info)))
                        .collect::<Result<Vec<Rule>>>()
                    {
                        Err(e) => {
                            log::warn!("{} {}", connect_info.id(), e);
                            return (false, Some(HookResult::AuthResult(AuthResult::NotAuthorized)));
                        }
                        Ok(rules) => rules,
                    }
                } else {
                    Vec::new()
                };
                log::debug!("rules: {rules:?}");
                let expire_at =
                    token_data.claims.get("exp").and_then(|exp| exp.as_u64().map(Duration::from_secs));
                let auth_info = AuthInfo { superuser, expire_at, rules };
                return (false, Some(HookResult::AuthResult(AuthResult::Allow(superuser, Some(auth_info)))));
            }

            Parameter::ClientSubscribeCheckAcl(session, subscribe) => {
                log::debug!("ClientSubscribeCheckAcl auth-jwt");
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
                //If none of the rules match, continue executing the subsequent authentication chain.
            }

            Parameter::MessagePublishCheckAcl(session, publish) => {
                log::debug!("MessagePublishCheckAcl auth-jwt");
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
                //If none of the rules match, continue executing the subsequent authentication chain.
            }

            Parameter::ClientKeepalive(s, _) => {
                if let Some(auth) = &s.auth_info {
                    log::debug!("Keepalive auth-jwt, is_expired: {:?}", auth.is_expired());
                    if auth.is_expired() && self.cfg.read().await.disconnect_if_expiry {
                        if let Some(tx) = self.scx.extends.shared().await.entry(s.id().clone()).tx() {
                            if let Err(e) = tx.unbounded_send(Message::Closed(Reason::ConnectDisconnect(
                                Some(Disconnect::Other("JWT Auth expired".into())),
                            ))) {
                                log::warn!("{} {}", s.id(), e);
                            }
                        }
                    }
                }
            }

            _ => {
                log::error!("unimplemented, {param:?}")
            }
        }
        (true, acc)
    }
}
