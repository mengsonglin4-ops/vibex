use std::{net::IpAddr, time::Duration};

use reqwest::{Client, Url, header::HeaderValue};
use serde_json::{Value, json};
use vibex_core::{
    AgentCredential, AgentCredentialStatus, ModelProviderAccountState, ModelProviderCatalogEntry,
    ModelProviderCredentialReference, ModelProviderEndpointKind, ModelProviderExtraBody,
    ModelProviderProfile, ModelProviderProfileStatus, ModelProviderProxyPolicy, ModelReviewTarget,
    ProviderProfile, ProviderProfileStatus, ProviderSecretBackend, ProviderSecretKind,
    ProviderSecretSetupState, RequestId, VibexError, VibexResult,
    WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS, WIRE_PROTOCOL_OPENAI_RESPONSES,
};
use vibex_db::{
    DbConnection, ModelProviderAccountStateRepository, ModelProviderProfileRepository,
    ProviderProfileRepository,
};

use crate::ProviderConfigService;

const MAX_RESPONSE_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wire {
    Responses,
    Chat,
}

impl Wire {
    fn declared(id: &str) -> VibexResult<Self> {
        match id {
            WIRE_PROTOCOL_OPENAI_RESPONSES => Ok(Self::Responses),
            WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS => Ok(Self::Chat),
            _ => Err(unsupported()),
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Responses => "/responses",
            Self::Chat => "/chat/completions",
        }
    }

    fn body(self, model: &str, text: &str) -> Value {
        let message = json!([{"role": "user", "content": text}]);
        match self {
            Self::Responses => {
                json!({"model": model, "input": message, "stream": false, "store": false})
            }
            Self::Chat => json!({"model": model, "messages": message, "stream": false}),
        }
    }

    fn read_text(self, body: &[u8]) -> Option<String> {
        let value: Value = serde_json::from_slice(body).ok()?;
        if !value["error"].is_null() {
            return None;
        }
        let text = match self {
            Self::Chat => {
                let [choice] = value["choices"].as_array()?.as_slice() else {
                    return None;
                };
                let message = &choice["message"];
                if message["role"] != "assistant"
                    || !(message["tool_calls"].is_null()
                        || message["tool_calls"].as_array().is_some_and(Vec::is_empty))
                    || !message["function_call"].is_null()
                    || !message["refusal"].is_null()
                    || (!choice["finish_reason"].is_null() && choice["finish_reason"] != "stop")
                {
                    return None;
                }
                message["content"].as_str()?.to_owned()
            }
            Self::Responses => {
                if !value["status"].is_null() && value["status"] != "completed" {
                    return None;
                }
                let mut text = String::new();
                for item in value["output"].as_array()? {
                    if item["type"] == "reasoning" {
                        continue;
                    }
                    if item["type"] != "message"
                        || item["role"] != "assistant"
                        || (!item["status"].is_null() && item["status"] != "completed")
                    {
                        return None;
                    }
                    for part in item["content"].as_array()? {
                        if part["type"] != "output_text" {
                            return None;
                        }
                        text.push_str(part["text"].as_str()?);
                    }
                }
                text
            }
        };
        (!text.trim().is_empty()).then_some(text)
    }
}

#[derive(PartialEq, Eq)]
struct Route {
    wire: Wire,
    url: Url,
    direct: bool,
    credential_id: Option<RequestId>,
    connect_timeout: Duration,
    request_timeout: Duration,
}

pub struct PreparedModelReview {
    service: ProviderConfigService,
    target: ModelReviewTarget,
    route: Route,
}

fn unsupported() -> VibexError {
    VibexError::capability(
        "model_review_configuration_unsupported",
        "Model review requires an unambiguous Responses or Chat Completions endpoint and supported provider settings",
    )
}

fn unavailable() -> VibexError {
    VibexError::conflict(
        "model_review_target_changed",
        "The review target is unavailable or changed; select it again",
    )
}

fn credential_error() -> VibexError {
    VibexError::validation(
        "model_review_credential_unavailable",
        "No usable API key is configured for this review model",
    )
}

fn proxy_error() -> VibexError {
    VibexError::capability(
        "model_review_proxy_unsupported",
        "Model review supports only the system proxy or an explicitly direct connection; custom provider and per-key proxies are not supported",
    )
}

fn config_read_error(_: VibexError) -> VibexError {
    VibexError::storage(
        "model_review_config_read_failed",
        "Could not read the local model review configuration",
    )
}

fn transport_error(error: reqwest::Error) -> VibexError {
    if error.is_timeout() {
        VibexError::provider("model_review_timeout", "The model review request timed out")
    } else {
        VibexError::provider(
            "model_review_transport_failed",
            "The model review request could not be completed",
        )
    }
}

fn secure_url(raw: &str, wire: Wire, direct: bool) -> VibexResult<Url> {
    let mut url = Url::parse(raw).map_err(|_| unsupported())?;
    let authority = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .ok_or_else(unsupported)?
        .split('/')
        .next()
        .unwrap_or_default();
    if raw
        .bytes()
        .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control() || byte == b'\\')
        || authority.is_empty()
        || authority.contains('@')
        || !url.has_host()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || !(url.scheme() == "https"
            || (url.scheme() == "http" && direct && literal_loopback(&url)))
    {
        return Err(unsupported());
    }
    let path = url.path().trim_end_matches('/');
    let complete = [Wire::Responses, Wire::Chat]
        .into_iter()
        .find(|known| path.ends_with(known.path()));
    let path = match complete {
        Some(known) if known == wire => path.to_owned(),
        Some(_) => return Err(unsupported()),
        None => format!(
            "{}{}",
            if path.is_empty() { "/v1" } else { path },
            wire.path()
        ),
    };
    url.set_path(&path);
    Ok(url)
}

fn literal_loopback(url: &Url) -> bool {
    url.host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback())
}

fn endpoint(
    profile: &ModelProviderProfile,
    model: &ModelProviderCatalogEntry,
    legacy: Option<&ProviderProfile>,
) -> VibexResult<(Wire, Url)> {
    let apis = || {
        profile
            .endpoints
            .iter()
            .filter(|endpoint| endpoint.kind == ModelProviderEndpointKind::Api)
    };
    let mut model_pins = model.metadata.iter().filter(|entry| entry.key == "wireApi");
    let model_pin = model_pins.next().map(|entry| entry.value.as_str());
    if model_pins.next().is_some() {
        return Err(unsupported());
    }
    let legacy_pin = legacy.and_then(|legacy| {
        legacy
            .configured_models
            .iter()
            .find(|entry| entry.id == model.id)
            .and_then(|entry| entry.wire_api)
            .map(|wire| wire.wire_protocol_id())
            .or_else(|| {
                legacy
                    .provider_options
                    .entries
                    .iter()
                    .find(|entry| entry.key == "wireApi")
                    .map(|entry| match entry.value.as_str() {
                        "responses" => WIRE_PROTOCOL_OPENAI_RESPONSES,
                        "chat" | "chat_completions" => WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS,
                        pin => pin,
                    })
            })
    });
    if matches!((model_pin, legacy_pin), (Some(model), Some(legacy)) if model != legacy) {
        return Err(unsupported());
    }
    let pin = match model_pin.or(legacy_pin) {
        Some(pin) => pin,
        None => {
            let mut declared = apis().filter_map(|endpoint| endpoint.wire_protocol_id.as_deref());
            let only = declared.next().ok_or_else(unsupported)?;
            if declared.next().is_some() {
                return Err(unsupported());
            }
            only
        }
    };
    let wire = Wire::declared(pin)?;
    let typed = apis().any(|endpoint| endpoint.wire_protocol_id.as_deref() == Some(pin));
    let mut candidates =
        apis().filter(|endpoint| endpoint.wire_protocol_id.as_deref() == typed.then_some(pin));
    let chosen = candidates.next().ok_or_else(unsupported)?;
    if candidates.next().is_some() {
        return Err(unsupported());
    }
    Ok((
        wire,
        secure_url(
            &chosen.url,
            wire,
            profile.proxy_policy == ModelProviderProxyPolicy::Disabled,
        )?,
    ))
}

fn scoped_key<'a>(
    profile: &'a ModelProviderProfile,
    model: &str,
    wire: Wire,
    states: &[ModelProviderAccountState],
    legacy: Option<&ProviderProfile>,
) -> Option<&'a ModelProviderCredentialReference> {
    let now = vibex_core::unix_timestamp_ms();
    profile.credentials.iter().find(|key| {
        let AgentCredential::ApiKey {
            secret,
            target_hint,
        } = &key.credential
        else {
            return false;
        };
        if target_hint.as_deref().is_some_and(|hint| {
            vibex_core::is_model_provider_wire_protocol(hint)
                && Wire::declared(hint).ok() != Some(wire)
        }) {
            return false;
        }
        matches!(
            key.status,
            AgentCredentialStatus::Ready | AgentCredentialStatus::Referenced
        ) && matches!(
            secret.setup_state,
            ProviderSecretSetupState::Available | ProviderSecretSetupState::Referenced
        ) && matches!(
            secret.backend,
            ProviderSecretBackend::OsKeychain
                | ProviderSecretBackend::HostFile
                | ProviderSecretBackend::Environment
        ) && (key.models.is_empty() || key.models.iter().any(|allowed| allowed == model))
            && states
                .iter()
                .filter(|state| state.credential_id == key.id)
                .all(|state| state.is_ready_at(now))
            && secret.legacy_secret_reference_id.as_ref().is_none_or(|id| {
                legacy.is_some_and(|legacy| {
                    legacy.secrets.iter().any(|reference| {
                        &reference.id == id
                            && reference.backend == secret.backend
                            && reference.lookup_key == secret.lookup_key
                            && reference.setup_state == secret.setup_state
                            && matches!(
                                reference.secret_kind,
                                ProviderSecretKind::ApiKey | ProviderSecretKind::AuthToken
                            )
                    })
                })
            })
    })
}

fn route(
    profile: &ModelProviderProfile,
    model: &ModelProviderCatalogEntry,
    states: &[ModelProviderAccountState],
    legacy: Option<&ProviderProfile>,
) -> VibexResult<Route> {
    let metadata = &profile.metadata;
    if profile.status != ModelProviderProfileStatus::Enabled
        || !model.enabled
        || profile.deleted_at_ms.is_some()
        || (profile.legacy_provider_profile_id.is_some() && legacy.is_none())
        || legacy.is_some_and(|legacy| {
            legacy.status != ProviderProfileStatus::Enabled
                || legacy.deleted_at_ms.is_some()
                || crate::is_internal_profile_for_listing(legacy)
                || legacy
                    .configured_models
                    .iter()
                    .any(|entry| entry.id == model.id && !entry.enabled)
        })
    {
        return Err(unavailable());
    }
    if legacy.is_some_and(|legacy| legacy.network_defaults.proxy_url.is_some())
        || matches!(profile.proxy_policy, ModelProviderProxyPolicy::Endpoint(_))
        || profile
            .endpoints
            .iter()
            .any(|endpoint| endpoint.kind == ModelProviderEndpointKind::Proxy)
    {
        return Err(proxy_error());
    }
    if metadata.only_through_route_groups
        || metadata.tls_accept_invalid
        || !profile.headers.is_empty()
        || !matches!(
            metadata.auth_header_mode.as_deref(),
            None | Some("auto" | "bearer")
        )
        || !matches!(metadata.user_agent_mode.as_deref(), None | Some("default"))
        || !ModelProviderExtraBody::from_metadata(metadata)
            .map_err(|_| unsupported())?
            .is_empty()
        || metadata
            .model_aliases
            .iter()
            .any(|alias| alias.alias == model.id)
        || profile
            .endpoints
            .iter()
            .any(|endpoint| endpoint.kind == ModelProviderEndpointKind::Auth)
    {
        return Err(unsupported());
    }
    let (wire, url) = endpoint(profile, model, legacy)?;
    let direct = profile.proxy_policy == ModelProviderProxyPolicy::Disabled;
    let credential_id = if profile.credentials.is_empty() && direct && literal_loopback(&url) {
        None
    } else {
        let key =
            scoped_key(profile, &model.id, wire, states, legacy).ok_or_else(credential_error)?;
        if key.proxy_url.is_some() {
            return Err(proxy_error());
        }
        Some(key.id.clone())
    };
    let timeout = |configured: Option<u64>, cap| match configured {
        Some(0) => Err(unsupported()),
        value => Ok(Duration::from_millis(value.unwrap_or(cap).min(cap))),
    };
    Ok(Route {
        wire,
        url,
        direct,
        credential_id,
        connect_timeout: timeout(metadata.connect_timeout_ms, 10_000)?,
        request_timeout: timeout(metadata.request_timeout_ms, 120_000)?,
    })
}

fn legacy_profile(
    conn: &DbConnection,
    profile: &ModelProviderProfile,
) -> VibexResult<Option<ProviderProfile>> {
    let Some(id) = &profile.legacy_provider_profile_id else {
        return Ok(None);
    };
    ProviderProfileRepository::get(conn, id).map_err(config_read_error)
}

impl ProviderConfigService {
    pub fn model_review_choices(&self) -> VibexResult<Vec<ModelReviewTarget>> {
        let conn = self.open_connection().map_err(config_read_error)?;
        let mut choices = Vec::new();
        for profile in ModelProviderProfileRepository::list(&conn).map_err(config_read_error)? {
            if profile.status != ModelProviderProfileStatus::Enabled {
                continue;
            }
            let legacy = legacy_profile(&conn, &profile)?;
            let states = ModelProviderAccountStateRepository::list(&conn, &profile.id)
                .map_err(config_read_error)?;
            for model in &profile.configured_models {
                if route(&profile, model, &states, legacy.as_ref()).is_ok() {
                    choices.push(ModelReviewTarget {
                        provider_id: profile.id.clone(),
                        provider_name: profile.display_name.clone(),
                        model_id: model.id.clone(),
                        revision: profile.revision,
                    });
                }
            }
        }
        Ok(choices)
    }

    fn review_configuration(
        &self,
        target: &ModelReviewTarget,
        prepared: Option<&Route>,
    ) -> VibexResult<(Route, Option<String>)> {
        let conn = self.open_connection().map_err(config_read_error)?;
        let profile = ModelProviderProfileRepository::get(&conn, &target.provider_id)
            .map_err(config_read_error)?
            .filter(|profile| {
                profile.revision == target.revision && profile.display_name == target.provider_name
            })
            .ok_or_else(unavailable)?;
        let legacy = legacy_profile(&conn, &profile)?;
        let model = profile
            .configured_models
            .iter()
            .find(|model| model.id == target.model_id)
            .ok_or_else(unavailable)?;
        let states = ModelProviderAccountStateRepository::list(&conn, &profile.id)
            .map_err(config_read_error)?;
        let route = route(&profile, model, &states, legacy.as_ref())?;
        if prepared.is_some_and(|expected| expected != &route) {
            return Err(unavailable());
        }
        if prepared.is_none() {
            return Ok((route, None));
        }
        let key = route
            .credential_id
            .as_ref()
            .map(|id| {
                let key = profile
                    .credentials
                    .iter()
                    .find(|key| &key.id == id)
                    .ok_or_else(credential_error)?;
                crate::shared_provider_credential_secret(key)
                    .map_err(|_| credential_error())?
                    .filter(|secret| {
                        !secret.is_empty() && secret.bytes().all(|byte| byte.is_ascii_graphic())
                    })
                    .ok_or_else(credential_error)
            })
            .transpose()?;
        Ok((route, key))
    }

    pub fn prepare_model_review(
        &self,
        target: &ModelReviewTarget,
    ) -> VibexResult<PreparedModelReview> {
        let (route, _) = self.review_configuration(target, None)?;
        Ok(PreparedModelReview {
            service: self.clone(),
            target: target.clone(),
            route,
        })
    }
}

impl PreparedModelReview {
    pub async fn send(self, text: &str) -> VibexResult<String> {
        let model_id = self.target.model_id.clone();
        let (route, key) = tokio::task::spawn_blocking(move || {
            self.service
                .review_configuration(&self.target, Some(&self.route))
        })
        .await
        .map_err(|_| {
            VibexError::storage(
                "model_review_config_task_failed",
                "Could not load the local review configuration",
            )
        })??;
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(route.connect_timeout)
            .timeout(route.request_timeout);
        if route.direct {
            builder = builder.no_proxy();
        }
        let client = builder.build().map_err(transport_error)?;
        let mut request = client
            .post(route.url)
            .json(&route.wire.body(&model_id, text));
        if let Some(secret) = &key {
            let mut header = HeaderValue::from_str(&format!("Bearer {secret}"))
                .map_err(|_| credential_error())?;
            header.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, header);
        }
        let mut response = request.send().await.map_err(transport_error)?;
        if !response.status().is_success() {
            return Err(VibexError::provider(
                "model_review_http_failed",
                format!(
                    "The review provider returned HTTP {}",
                    response.status().as_u16()
                ),
            ));
        }
        let too_large = || {
            VibexError::provider(
                "model_review_response_too_large",
                "The model review response exceeded the size limit",
            )
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        route
            .wire
            .read_text(&bytes)
            .filter(|text| {
                key.as_ref()
                    .is_none_or(|secret| !text.contains(secret.as_str()))
            })
            .ok_or_else(|| {
                VibexError::provider(
                    "model_review_invalid_response",
                    "The provider did not return a usable text review",
                )
            })
    }
}

#[cfg(test)]
mod tests;
