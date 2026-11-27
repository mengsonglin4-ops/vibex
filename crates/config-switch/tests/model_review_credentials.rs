use std::{io::ErrorKind, path::Path, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    time::timeout,
};
use vibex_config_switch::{
    PreparedModelReview, ProviderConfigService,
    secrets::{
        ProviderSecretStoreConfig, configure_provider_secret_store, delete_provider_secret,
        provider_secret_write_backend, resolve_provider_secret_reference, store_provider_secret,
    },
};
use vibex_core::{
    AgentCredential, AgentCredentialStatus, ModelProviderCatalogEntry,
    ModelProviderCredentialReference, ModelProviderEndpoint, ModelProviderEndpointKind,
    ModelProviderProfileCreateRequest, ModelProviderProfileStatus,
    ModelProviderProfileUpdateRequest, ModelProviderProxyPolicy, ModelReviewTarget,
    ProjectionSecretReference, ProviderSecretBackend, ProviderSecretSetupState, RequestId,
    VibexError, VibexResult, WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS, WIRE_PROTOCOL_OPENAI_RESPONSES,
};

const MODEL: &str = "selected-review-model";
const OTHER_MODEL: &str = "unselected-default-model";
const SECRET: &str = "synthetic-selected-review-key-never-a-real-credential";
const OTHER_SECRET: &str = "synthetic-unselected-model-key-never-a-real-credential";
const FALLBACK_SECRET: &str = "synthetic-fallback-key-never-a-real-credential";
const ERROR_BODY: &str = "private-upstream-error-body";
const TEXT: &str = "Review only this diff:\r\n+\t\"quoted\" \\ \u{1f642}\n-  \n";
const IO_TIMEOUT: Duration = Duration::from_secs(5);

struct Mock {
    listener: TcpListener,
    pending: std::net::TcpListener,
    base: String,
}

impl Mock {
    fn new() -> Self {
        let pending = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        pending.set_nonblocking(true).unwrap();
        let base = format!("http://{}", pending.local_addr().unwrap());
        let listener = TcpListener::from_std(pending.try_clone().unwrap()).unwrap();
        Self {
            listener,
            pending,
            base,
        }
    }

    fn assert_no_request(&self) {
        assert_eq!(
            self.pending.accept().unwrap_err().kind(),
            ErrorKind::WouldBlock,
            "a preview, rejected send, or completed request must not open another connection"
        );
    }

    async fn exchange(
        &self,
        prepared: PreparedModelReview,
        path: &str,
        expected_body: &Value,
        status: &str,
        response_body: Value,
    ) -> VibexResult<String> {
        let response_body = response_body.to_string();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let (result, ()) = timeout(IO_TIMEOUT, async {
            tokio::join!(prepared.send(TEXT), async {
                let (stream, peer) = self.listener.accept().await.unwrap();
                assert!(peer.ip().is_loopback());
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                assert_eq!(request_line, format!("POST {path} HTTP/1.1\r\n"));
                let mut headers = Vec::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                    assert!(line.len() < 4096);
                    if line == "\r\n" {
                        break;
                    }
                    assert!(headers.len() < 32);
                    headers.push(line.trim_end().to_owned());
                }
                let authorization = headers
                    .iter()
                    .filter_map(|header| header.split_once(':'))
                    .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.trim())
                    .collect::<Vec<_>>();
                assert_eq!(authorization, vec![format!("Bearer {SECRET}")]);
                let content_length = headers
                    .iter()
                    .filter_map(|header| header.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .unwrap()
                    .1
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                assert!(content_length < 4096);
                let mut body = vec![0; content_length];
                reader.read_exact(&mut body).await.unwrap();
                assert_eq!(
                    &serde_json::from_slice::<Value>(&body).unwrap(),
                    expected_body
                );
                reader
                    .into_inner()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            })
        })
        .await
        .expect("the loopback request and mock response must finish within five seconds");
        self.assert_no_request();
        result
    }
}

fn stored_credential(model: &str, secret: &str) -> ModelProviderCredentialReference {
    let lookup_key = format!("synthetic-model-review-{}", RequestId::new().as_str());
    store_provider_secret(&lookup_key, secret).unwrap();
    assert_eq!(resolve(&lookup_key).as_deref(), Some(secret));
    ModelProviderCredentialReference {
        id: RequestId::new(),
        status: AgentCredentialStatus::Ready,
        models: vec![model.to_owned()],
        credential: AgentCredential::ApiKey {
            secret: ProjectionSecretReference {
                id: RequestId::new(),
                backend: ProviderSecretBackend::HostFile,
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

fn lookup_key(credential: &ModelProviderCredentialReference) -> &str {
    &credential.credential.secret_reference().unwrap().lookup_key
}

fn resolve(lookup_key: &str) -> Option<String> {
    resolve_provider_secret_reference(
        ProviderSecretBackend::HostFile,
        ProviderSecretSetupState::Available,
        lookup_key,
    )
    .unwrap()
}

fn stored_secrets(root: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(root.join("provider-secrets.json")).unwrap()).unwrap()
}

fn profile(
    base: &str,
    wire: &str,
    credentials: Vec<ModelProviderCredentialReference>,
) -> ModelProviderProfileCreateRequest {
    ModelProviderProfileCreateRequest {
        display_name: "Synthetic HostFile review fixture".into(),
        vendor_hint: None,
        endpoints: vec![ModelProviderEndpoint {
            id: "loopback".into(),
            kind: ModelProviderEndpointKind::Api,
            url: base.into(),
            wire_protocol_id: Some(wire.into()),
        }],
        proxy_policy: ModelProviderProxyPolicy::Disabled,
        credentials,
        configured_models: [OTHER_MODEL, MODEL]
            .into_iter()
            .map(|model| ModelProviderCatalogEntry {
                id: model.into(),
                display_name: None,
                enabled: true,
                metadata: Vec::new(),
                capabilities: Default::default(),
            })
            .collect(),
        default_model_id: Some(OTHER_MODEL.into()),
        headers: Vec::new(),
        metadata: Default::default(),
        status: ModelProviderProfileStatus::Enabled,
    }
}

fn reply(wire: &str, text: &str) -> Value {
    match wire {
        WIRE_PROTOCOL_OPENAI_RESPONSES => json!({
            "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": text}
            ]}]
        }),
        WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS => json!({
            "choices": [{"finish_reason": "stop", "message": {
                "role": "assistant", "content": text, "tool_calls": []
            }}]
        }),
        _ => panic!("unexpected fixture protocol"),
    }
}

fn assert_redacted(error: &VibexError, code: &str) {
    assert_eq!(error.code, code);
    assert!(error.diagnostics.is_empty());
    for rendered in [
        error.to_string(),
        format!("{error:?}"),
        serde_json::to_string(error).unwrap(),
    ] {
        for forbidden in [SECRET, OTHER_SECRET, FALLBACK_SECRET, ERROR_BODY] {
            assert!(!rendered.contains(forbidden));
        }
    }
}

#[tokio::test]
#[cfg_attr(
    feature = "os-keychain",
    ignore = "requires an isolated --no-default-features config-switch test run"
)]
async fn model_review_credentials_use_host_file_and_fail_closed_after_preview() {
    assert!(
        !cfg!(feature = "os-keychain"),
        "run this integration target with --no-default-features: HostFile deletion also cleans the OS keychain when os-keychain is enabled"
    );
    let root = tempfile::Builder::new()
        .prefix("model-review-credentials-")
        .tempdir()
        .unwrap();
    configure_provider_secret_store(ProviderSecretStoreConfig::host_file(root.path()));
    assert_eq!(
        provider_secret_write_backend(),
        ProviderSecretBackend::HostFile
    );
    let credentials = vec![
        stored_credential(OTHER_MODEL, OTHER_SECRET),
        stored_credential(MODEL, SECRET),
        stored_credential(MODEL, FALLBACK_SECRET),
    ];
    let selected_key = lookup_key(&credentials[1]);
    let persisted = stored_secrets(root.path());
    assert_eq!(persisted.as_object().unwrap().len(), credentials.len());
    for (credential, secret) in credentials
        .iter()
        .zip([OTHER_SECRET, SECRET, FALLBACK_SECRET])
    {
        assert_eq!(persisted[lookup_key(credential)], secret);
    }

    for (wire, path, expected_body) in [
        (
            WIRE_PROTOCOL_OPENAI_RESPONSES,
            "/v1/responses",
            json!({"model": MODEL, "stream": false, "store": false,
                "input": [{"role": "user", "content": TEXT}]}),
        ),
        (
            WIRE_PROTOCOL_OPENAI_CHAT_COMPLETIONS,
            "/v1/chat/completions",
            json!({"model": MODEL, "stream": false,
                "messages": [{"role": "user", "content": TEXT}]}),
        ),
    ] {
        let mock = Mock::new();
        let replacement = Mock::new();
        let service = ProviderConfigService::new(root.path().join(format!("{wire}.db")));
        let mut config = profile(&mock.base, wire, credentials.clone());
        config.metadata.connect_timeout_ms = Some(1_000);
        config.metadata.request_timeout_ms = Some(3_000);
        let mut provider = service.create_model_provider_profile(config).unwrap();
        let target = ModelReviewTarget {
            provider_id: provider.id.clone(),
            provider_name: provider.display_name.clone(),
            model_id: MODEL.into(),
            revision: provider.revision,
        };
        assert!(service.model_review_choices().unwrap().contains(&target));
        let prepared = service.prepare_model_review(&target).unwrap();
        mock.assert_no_request();
        assert_eq!(
            mock.exchange(
                prepared,
                path,
                &expected_body,
                "200 OK",
                reply(wire, "review complete")
            )
            .await
            .unwrap(),
            "review complete"
        );

        let upstream_error = json!({"error": {"message": format!("{ERROR_BODY}: {SECRET}")}});
        for (status, body, expected_code) in [
            (
                "503 Unavailable",
                upstream_error.clone(),
                "model_review_http_failed",
            ),
            ("200 OK", upstream_error, "model_review_invalid_response"),
            (
                "200 OK",
                reply(wire, &format!("{ERROR_BODY}: Bearer {SECRET}")),
                "model_review_invalid_response",
            ),
        ] {
            let prepared = service.prepare_model_review(&target).unwrap();
            let error = mock
                .exchange(prepared, path, &expected_body, status, body)
                .await
                .unwrap_err();
            assert_redacted(&error, expected_code);
        }

        let prepared = service.prepare_model_review(&target).unwrap();
        delete_provider_secret(selected_key).unwrap();
        assert_eq!(resolve(selected_key), None);
        let persisted = stored_secrets(root.path());
        assert!(persisted.get(selected_key).is_none());
        assert_eq!(persisted[lookup_key(&credentials[0])], OTHER_SECRET);
        assert_eq!(persisted[lookup_key(&credentials[2])], FALLBACK_SECRET);
        let error = timeout(IO_TIMEOUT, prepared.send(TEXT))
            .await
            .unwrap()
            .unwrap_err();
        assert_redacted(&error, "model_review_credential_unavailable");
        mock.assert_no_request();
        let prepared_without_secret = service.prepare_model_review(&target).unwrap();
        let error = timeout(IO_TIMEOUT, prepared_without_secret.send(TEXT))
            .await
            .unwrap()
            .unwrap_err();
        assert_redacted(&error, "model_review_credential_unavailable");
        mock.assert_no_request();

        store_provider_secret(selected_key, SECRET).unwrap();
        assert_eq!(resolve(selected_key).as_deref(), Some(SECRET));
        let prepared = service.prepare_model_review(&target).unwrap();
        provider.endpoints[0].url = replacement.base.clone();
        let expected_revision = provider.revision;
        let updated = service
            .update_model_provider_profile(ModelProviderProfileUpdateRequest {
                profile: provider,
                expected_revision,
            })
            .unwrap();
        assert!(updated.revision > target.revision);
        let error = timeout(IO_TIMEOUT, prepared.send(TEXT))
            .await
            .unwrap()
            .unwrap_err();
        assert_redacted(&error, "model_review_target_changed");
        mock.assert_no_request();
        replacement.assert_no_request();
        let updated_target = ModelReviewTarget {
            revision: updated.revision,
            ..target
        };
        let prepared = service.prepare_model_review(&updated_target).unwrap();
        assert_eq!(
            replacement
                .exchange(
                    prepared,
                    path,
                    &expected_body,
                    "200 OK",
                    reply(wire, "review recovered")
                )
                .await
                .unwrap(),
            "review recovered"
        );
        mock.assert_no_request();
    }

    for credential in &credentials {
        let lookup_key = lookup_key(credential);
        delete_provider_secret(lookup_key).unwrap();
        assert_eq!(resolve(lookup_key), None);
    }
    assert_eq!(stored_secrets(root.path()), json!({}));
}
