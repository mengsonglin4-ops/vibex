use std::io::ErrorKind;

use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use vibex_core::{
    ModelProviderEndpoint, ModelProviderProfileCreateRequest, ModelProviderProfileUpdateRequest,
    ProjectionSecretReference, ProviderBindingMetadata,
};

use super::*;

const MODEL: &str = "selected-model";
const SECRET: &str = "mock-review-key-not-for-output";
const TEXT: &str = "Review exactly this:\r\n+\t\"quoted\" \\ \u{1f642}\n-  \n";

struct Mock {
    listener: TcpListener,
    base: String,
}

impl Mock {
    fn new() -> Self {
        let pending = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        pending.set_nonblocking(true).unwrap();
        let base = format!("http://{}", pending.local_addr().unwrap());
        let listener = TcpListener::from_std(pending).unwrap();
        Self { listener, base }
    }

    async fn assert_no_request(&self) {
        assert!(
            timeout(Duration::from_millis(50), self.listener.accept())
                .await
                .is_err(),
            "no connection may reach the mock during the observation window"
        );
    }

    async fn request(&self) -> (TcpStream, String, Vec<String>, Value) {
        let (stream, _) = self.listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut method_path = String::new();
        reader.read_line(&mut method_path).await.unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
            if line == "\r\n" {
                break;
            }
            headers.push(line.trim_end().to_owned());
        }
        let length = headers
            .iter()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<usize>()
            .unwrap();
        assert!(length < 4096);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        (
            reader.into_inner(),
            method_path,
            headers,
            serde_json::from_slice(&body).unwrap(),
        )
    }

    async fn reply(&self, reply: &str) -> (String, Vec<String>, Value) {
        let (mut stream, path, headers, body) = self.request().await;
        stream.write_all(reply.as_bytes()).await.unwrap();
        (path, headers, body)
    }
}

fn credential(model: &str) -> ModelProviderCredentialReference {
    let id = RequestId::new();
    let lookup_key = format!("mock-review-{}", id.as_str());
    crate::secrets::store_provider_secret(&lookup_key, SECRET).unwrap();
    ModelProviderCredentialReference {
        id,
        status: AgentCredentialStatus::Ready,
        models: vec![model.to_owned()],
        credential: AgentCredential::ApiKey {
            secret: ProjectionSecretReference {
                id: RequestId::new(),
                backend: ProviderSecretBackend::OsKeychain,
                setup_state: ProviderSecretSetupState::Available,
                lookup_key,
                redacted_hint: String::new(),
                revision: 1,
                legacy_secret_reference_id: None,
            },
            target_hint: None,
        },
        ..Default::default()
    }
}

fn profile(base: &str, wire: &str) -> ModelProviderProfileCreateRequest {
    ModelProviderProfileCreateRequest {
        display_name: "Review fixture".into(),
        vendor_hint: None,
        endpoints: vec![ModelProviderEndpoint {
            id: "selected".into(),
            kind: ModelProviderEndpointKind::Api,
            url: base.into(),
            wire_protocol_id: Some(wire.into()),
        }],
        proxy_policy: ModelProviderProxyPolicy::Disabled,
        credentials: vec![credential("not-this-model"), credential(MODEL)],
        configured_models: vec![ModelProviderCatalogEntry {
            id: MODEL.into(),
            display_name: None,
            enabled: true,
            metadata: Vec::new(),
            capabilities: Default::default(),
        }],
        default_model_id: None,
        headers: Vec::new(),
        metadata: Default::default(),
        status: ModelProviderProfileStatus::Enabled,
    }
}

fn fixture(
    request: ModelProviderProfileCreateRequest,
) -> (
    TempDir,
    ProviderConfigService,
    ModelProviderProfile,
    ModelReviewTarget,
) {
    let directory = tempfile::tempdir().unwrap();
    let service = ProviderConfigService::new(directory.path().join("review.db"));
    let profile = service.create_model_provider_profile(request).unwrap();
    let target = ModelReviewTarget {
        provider_id: profile.id.clone(),
        provider_name: profile.display_name.clone(),
        model_id: MODEL.into(),
        revision: profile.revision,
    };
    (directory, service, profile, target)
}

fn ok_json(body: Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn mock_no_request_returns_without_a_connection() {
    timeout(Duration::from_secs(1), Mock::new().assert_no_request())
        .await
        .unwrap();
}

#[tokio::test]
#[should_panic(expected = "no connection may reach the mock during the observation window")]
async fn mock_no_request_rejects_a_queued_connection() {
    let mock = Mock::new();
    let _stream = timeout(
        Duration::from_secs(1),
        TcpStream::connect(mock.listener.local_addr().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    mock.assert_no_request().await;
}

#[test]
fn chat_text_accepts_only_absent_null_or_empty_tool_calls() {
    let reply = json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"report!"}}]});
    for tool_calls in [None, Some(Value::Null), Some(json!([]))] {
        let mut response = reply.clone();
        if let Some(tool_calls) = tool_calls {
            response["choices"][0]["message"]["tool_calls"] = tool_calls;
        }
        assert_eq!(
            Wire::Chat.read_text(&serde_json::to_vec(&response).unwrap()),
            Some("report!".to_owned())
        );
    }
    for tool_calls in [json!([{}]), json!({}), json!(""), json!(false), json!(0)] {
        let mut response = reply.clone();
        response["choices"][0]["message"]["tool_calls"] = tool_calls;
        assert_eq!(
            Wire::Chat.read_text(&serde_json::to_vec(&response).unwrap()),
            None
        );
    }
}

#[test]
fn review_text_rejects_refusals_incomplete_and_malformed_replies() {
    for (wire, reply) in [
        (
            Wire::Chat,
            json!({"choices":[{"finish_reason":"length","message":{"role":"assistant","content":"partial"}}]}),
        ),
        (
            Wire::Chat,
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"ignored","refusal":"declined"}}]}),
        ),
        (
            Wire::Chat,
            json!({"choices":[{"message":{"role":"assistant","content":"  "}}]}),
        ),
        (
            Wire::Responses,
            json!({"status":"incomplete","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial"}]}]}),
        ),
        (Wire::Responses, json!({"status":"failed","output":[]})),
        (
            Wire::Responses,
            json!({"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"declined"}]}]}),
        ),
        (Wire::Responses, json!({"status":"completed","output":[]})),
    ] {
        assert_eq!(wire.read_text(&serde_json::to_vec(&reply).unwrap()), None);
    }
    for wire in [Wire::Chat, Wire::Responses] {
        assert_eq!(wire.read_text(b"not JSON"), None);
        assert_eq!(wire.read_text(b"null"), None);
        assert_eq!(wire.read_text(br#"{"error":{"message":"failure"}}"#), None);
    }
}

#[test]
fn review_timeouts_preserve_defaults_caps_and_reject_zero() {
    let (_dir, _service, mut provider, _) = fixture(profile(
        "https://provider.invalid/v1",
        WIRE_PROTOCOL_OPENAI_RESPONSES,
    ));
    for (connect, request, expected_connect, expected_request) in [
        (None, None, 10_000, 120_000),
        (Some(250), Some(500), 250, 500),
        (Some(20_000), Some(240_000), 10_000, 120_000),
    ] {
        provider.metadata.connect_timeout_ms = connect;
        provider.metadata.request_timeout_ms = request;
        let selected = route(&provider, &provider.configured_models[0], &[], None).unwrap();
        assert_eq!(
            selected.connect_timeout,
            Duration::from_millis(expected_connect)
        );
        assert_eq!(
            selected.request_timeout,
            Duration::from_millis(expected_request)
        );
    }
    for (connect, request) in [(Some(0), None), (None, Some(0))] {
        provider.metadata.connect_timeout_ms = connect;
        provider.metadata.request_timeout_ms = request;
        assert!(matches!(
            route(&provider, &provider.configured_models[0], &[], None),
            Err(error) if error.code == "model_review_configuration_unsupported"
        ));
    }
}

#[tokio::test]
async fn only_the_confirmed_text_and_selected_model_reach_the_declared_endpoint() {
    for (pin, endpoint, path, reply, expected) in [
        (
            WIRE_PROTOCOL_OPENAI_RESPONSES,
            "/custom/v1/",
            "/custom/v1/responses",
            json!({"status":"completed", "output":[{"type":"reasoning"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"report"},{"type":"output_text","text":"!"}]}]}),
            json!({"model":MODEL,"stream":false,"store":false,"input":[{"role":"user","content":TEXT}]}),
        ),
        (
            WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS,
            "/v1/chat/completions",
            "/v1/chat/completions",
            json!({"choices":[{"finish_reason":"stop","message":{"role":"assistant","content":"report!","tool_calls":[]}}]}),
            json!({"model":MODEL,"stream":false,"messages":[{"role":"user","content":TEXT}]}),
        ),
    ] {
        let mock = Mock::new();
        let unused = Mock::new();
        let mut config = profile(&format!("{}{endpoint}", mock.base), pin);
        config.endpoints.insert(
            0,
            ModelProviderEndpoint {
                id: "generic".into(),
                kind: ModelProviderEndpointKind::Api,
                url: unused.base.clone(),
                wire_protocol_id: None,
            },
        );
        config.configured_models[0]
            .metadata
            .push(ProviderBindingMetadata {
                key: "wireApi".into(),
                value: pin.into(),
            });
        let (_dir, service, profile, target) = fixture(config);
        assert_eq!(
            service.model_review_choices().unwrap(),
            vec![target.clone()]
        );
        let prepared = service.prepare_model_review(&target).unwrap();
        assert_eq!(
            prepared.route.credential_id.as_ref(),
            Some(&profile.credentials[1].id)
        );
        mock.assert_no_request().await;
        let response = ok_json(reply);
        let (result, (request_line, headers, body)) = timeout(Duration::from_secs(5), async {
            tokio::join!(prepared.send(TEXT), mock.reply(&response))
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), "report!");
        assert_eq!(request_line.trim_end(), format!("POST {path} HTTP/1.1"));
        assert!(
            headers
                .iter()
                .any(|line| line.eq_ignore_ascii_case(&format!("authorization: Bearer {SECRET}")))
        );
        assert_eq!(body, expected);
        mock.assert_no_request().await;
        unused.assert_no_request().await;
    }
}

#[tokio::test]
async fn redirect_http_failure_invalid_text_and_oversized_bodies_are_terminal_and_redacted() {
    let mock = Mock::new();
    let unused = Mock::new();
    let (_dir, service, _, target) =
        fixture(profile(&mock.base, WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS));
    let big = "x".repeat(MAX_RESPONSE_BYTES + 1);
    for (expected_code, reply) in [
        (
            "model_review_http_failed",
            format!(
                "HTTP/1.1 307 Temporary Redirect\r\nLocation: {}/other\r\nContent-Length: 0\r\n\r\n",
                unused.base
            ),
        ),
        (
            "model_review_http_failed",
            format!(
                "HTTP/1.1 503 Busy\r\nContent-Length: {}\r\n\r\n{SECRET}",
                SECRET.len()
            ),
        ),
        (
            "model_review_invalid_response",
            ok_json(json!({"choices":[{"message":{"role":"assistant","content":SECRET}}]})),
        ),
        (
            "model_review_invalid_response",
            ok_json(
                json!({"choices":[{"message":{"role":"assistant","content":"ignored","tool_calls":[{}]}}]}),
            ),
        ),
        (
            "model_review_response_too_large",
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", big.len()),
        ),
        (
            "model_review_response_too_large",
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:X}\r\n{}\r\n1\r\nx\r\n0\r\n\r\n",
                MAX_RESPONSE_BYTES,
                &big[..MAX_RESPONSE_BYTES]
            ),
        ),
    ] {
        let prepared = service.prepare_model_review(&target).unwrap();
        let (result, _) = timeout(Duration::from_secs(5), async {
            tokio::join!(prepared.send(TEXT), mock.reply(&reply))
        })
        .await
        .unwrap();
        let error = result.unwrap_err();
        assert_eq!(error.code, expected_code);
        assert!(!format!("{error:?}").contains(SECRET));
        assert!(error.diagnostics.is_empty());
        mock.assert_no_request().await;
        unused.assert_no_request().await;
    }
}

#[tokio::test]
async fn stale_selection_revoked_key_and_disabled_model_never_connect() {
    let mock = Mock::new();
    let (_dir, service, mut profile, target) =
        fixture(profile(&mock.base, WIRE_PROTOCOL_OPENAI_RESPONSES));
    let prepared = service.prepare_model_review(&target).unwrap();
    let pending = service.prepare_model_review(&target).unwrap();
    let mut state =
        ModelProviderAccountState::ready(profile.id.clone(), profile.credentials[1].id.clone());
    state.enabled = false;
    ModelProviderAccountStateRepository::upsert(&service.open_connection().unwrap(), &state)
        .unwrap();
    assert!(service.model_review_choices().unwrap().is_empty());
    assert_eq!(
        timeout(Duration::from_secs(5), prepared.send(TEXT))
            .await
            .unwrap()
            .unwrap_err()
            .code,
        "model_review_credential_unavailable"
    );
    profile.configured_models[0].enabled = false;
    let expected_revision = profile.revision;
    let updated = service
        .update_model_provider_profile(ModelProviderProfileUpdateRequest {
            profile,
            expected_revision,
        })
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), pending.send(TEXT))
            .await
            .unwrap()
            .unwrap_err()
            .code,
        "model_review_target_changed"
    );
    assert!(
        service
            .prepare_model_review(&ModelReviewTarget {
                revision: updated.revision,
                ..target
            })
            .is_err()
    );
    mock.assert_no_request().await;
}

#[tokio::test]
async fn timeout_and_dropping_an_inflight_review_close_the_request() {
    let mock = Mock::new();
    for cancel in [false, true] {
        let mut config = profile(&mock.base, WIRE_PROTOCOL_OPENAI_RESPONSES);
        config.metadata.request_timeout_ms = Some(if cancel { 10_000 } else { 500 });
        let (_dir, service, _, target) = fixture(config);
        let mut send = Box::pin(service.prepare_model_review(&target).unwrap().send(TEXT));
        let (mut stream, ..) = timeout(Duration::from_secs(5), async {
            tokio::select! { request = mock.request() => request, result = &mut send => panic!("request finished before mock read: {result:?}") }
        }).await.unwrap();
        let observed_request = std::time::Instant::now();
        if cancel {
            drop(send);
        } else {
            let error = timeout(Duration::from_secs(3), send)
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.code, "model_review_timeout");
            assert!(observed_request.elapsed() < Duration::from_secs(3));
        }
        let mut byte = [0];
        let closed = timeout(Duration::from_secs(3), stream.read(&mut byte))
            .await
            .unwrap();
        assert!(
            matches!(closed, Ok(0))
                || closed.is_err_and(|error| matches!(
                    error.kind(),
                    ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
                ))
        );
        mock.assert_no_request().await;
    }
}

#[tokio::test]
async fn review_timeout_covers_a_stalled_body_after_headers() {
    let mock = Mock::new();
    let mut config = profile(&mock.base, WIRE_PROTOCOL_OPENAI_RESPONSES);
    config.metadata.request_timeout_ms = Some(500);
    let (_dir, service, _, target) = fixture(config);
    let mut send = Box::pin(service.prepare_model_review(&target).unwrap().send(TEXT));
    let (mut stream, ..) = timeout(Duration::from_secs(5), async {
        tokio::select! {
            request = mock.request() => request,
            result = &mut send => panic!("request finished before mock read: {result:?}"),
        }
    })
    .await
    .unwrap();
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 128\r\n\r\n{",
        )
        .await
        .unwrap();
    let observed_headers = std::time::Instant::now();
    let error = timeout(Duration::from_secs(3), send)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, "model_review_timeout");
    assert!(observed_headers.elapsed() < Duration::from_secs(3));
    let mut byte = [0];
    let closed = timeout(Duration::from_secs(3), stream.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(0))
            || closed.is_err_and(|error| matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
            ))
    );
    mock.assert_no_request().await;
}

#[test]
fn unsafe_urls_and_conflicting_operation_paths_are_rejected() {
    for url in [
        "file:///review",
        "http://example.test/v1",
        "http://localhost.example.test",
        "https://user@example.test/v1",
        "https://@example.test/v1",
        "https://example.test/?key=private",
        "https://example.test/#fragment",
        "https://example.test/\r\n",
        "https://example.test/v1/chat/completions",
    ] {
        let error = secure_url(url, Wire::Responses, true).unwrap_err();
        assert!(!format!("{error:?}").contains(url));
    }
    assert!(secure_url("http://127.0.0.1", Wire::Chat, false).is_err());
    assert_eq!(
        secure_url("https://example.test", Wire::Chat, false)
            .unwrap()
            .path(),
        "/v1/chat/completions"
    );
    assert_eq!(
        secure_url("http://[::1]:80", Wire::Responses, true)
            .unwrap()
            .path(),
        "/v1/responses"
    );
}

#[test]
fn legacy_codex_wire_pin_selects_the_configured_base_without_an_agent() {
    let (_dir, _, mut profile, _) = fixture(profile(
        "https://provider.invalid/v1",
        WIRE_PROTOCOL_OPENAI_RESPONSES,
    ));
    profile.endpoints[0].wire_protocol_id = None;
    let mut legacy = ProviderProfile::local_default(vibex_core::ProviderKind::Codex);
    legacy
        .provider_options
        .entries
        .push(ProviderBindingMetadata {
            key: "wireApi".into(),
            value: "responses".into(),
        });
    let (wire, url) = endpoint(&profile, &profile.configured_models[0], Some(&legacy)).unwrap();
    assert!(wire == Wire::Responses);
    assert_eq!(url.as_str(), "https://provider.invalid/v1/responses");
}

#[tokio::test]
async fn a_missing_secret_cannot_fall_through_to_another_key_or_anonymous_request() {
    eprintln!("[model-review] missing-secret: creating loopback listener");
    let mock = Mock::new();
    eprintln!("[model-review] missing-secret: storing synthetic credentials");
    let mut config = profile(&mock.base, WIRE_PROTOCOL_OPENAI_RESPONSES);
    config.credentials.push(credential(MODEL));
    eprintln!("[model-review] missing-secret: creating isolated database fixture");
    let (_dir, service, profile, target) = fixture(config);
    eprintln!("[model-review] missing-secret: preparing selected route");
    let prepared = service.prepare_model_review(&target).unwrap();
    let secret = profile.credentials[1]
        .credential
        .secret_reference()
        .unwrap();
    crate::secrets::delete_provider_secret(&secret.lookup_key).unwrap();
    eprintln!("[model-review] missing-secret: checking revoked prepared route");
    assert_eq!(
        timeout(Duration::from_secs(5), prepared.send(TEXT))
            .await
            .unwrap()
            .unwrap_err()
            .code,
        "model_review_credential_unavailable"
    );
    let preview_without_secret = service.prepare_model_review(&target).unwrap();
    eprintln!("[model-review] missing-secret: checking new preview without secret");
    assert_eq!(
        timeout(Duration::from_secs(5), preview_without_secret.send(TEXT))
            .await
            .unwrap()
            .unwrap_err()
            .code,
        "model_review_credential_unavailable"
    );
    eprintln!("[model-review] missing-secret: checking no connection");
    mock.assert_no_request().await;
    eprintln!("[model-review] missing-secret: completed");
}

#[test]
fn unsafe_settings_unavailable_credentials_and_ambiguous_protocols_are_not_choices() {
    let original = profile(
        "https://provider.invalid/v1",
        WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS,
    );
    type Change = fn(&mut ModelProviderProfileCreateRequest);
    for change in [
        (|request| request.status = ModelProviderProfileStatus::Disabled) as Change,
        |request| request.metadata.tls_accept_invalid = true,
        |request| {
            request.metadata.extra_body_json =
                Some(r#"{"previous_response_id":"another-turn"}"#.into())
        },
        |request| request.metadata.only_through_route_groups = true,
        |request| {
            request.proxy_policy = ModelProviderProxyPolicy::Endpoint("http://proxy.invalid".into())
        },
        |request| request.credentials[1].proxy_url = Some("http://proxy.invalid".into()),
        |request| request.credentials[1].status = AgentCredentialStatus::Expired,
        |request| request.credentials[1].models = vec!["another-model".into()],
        |request| request.credentials.clear(),
        |request| request.endpoints[0].wire_protocol_id = None,
        |request| {
            request.endpoints[0].wire_protocol_id =
                Some(vibex_core::WIRE_PROTOCOL_ANTHROPIC_MESSAGES.into())
        },
        |request| {
            let mut second = request.endpoints[0].clone();
            second.id = "other".into();
            second.wire_protocol_id = Some(WIRE_PROTOCOL_OPENAI_RESPONSES.into());
            request.endpoints.push(second);
        },
    ] {
        let mut config = original.clone();
        change(&mut config);
        let (_dir, service, _, target) = fixture(config);
        assert!(service.model_review_choices().unwrap().is_empty());
        assert!(service.prepare_model_review(&target).is_err());
    }
}
