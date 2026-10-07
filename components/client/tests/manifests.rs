//! Tests for the manifests fetched by `client`, against a scripted registry.

use test_harness::{
    Digest, DigestAlgorithm, ErrorCode, Harness, Manifest, Reference, RegistryResponse, sha256,
};

const MANIFEST: &[u8] = br#"{
    "schemaVersion": 2,
    "mediaType": "application/vnd.oci.image.manifest.v1+json",
    "config": {
        "mediaType": "application/vnd.wasm.config.v0+json",
        "digest": "sha256:80d83bbdaa82cff96584c99217c29fd17bc7e5f0424c0cb26aa3831ea18132b4",
        "size": 345
    },
    "layers": [
        {
            "mediaType": "application/wasm",
            "digest": "sha256:ee7ff5c9588e997b4a54b6d351b52a5ca4f6980377f59e48c778f48a23b483db",
            "size": 1894585
        }
    ]
}"#;

/// Whether the digests are the same, the generated `Digest` has no `PartialEq`.
fn same_digest(a: &Digest, b: &Digest) -> bool {
    matches!(
        (&a.algorithm, &b.algorithm),
        (DigestAlgorithm::Sha256, DigestAlgorithm::Sha256)
            | (DigestAlgorithm::Sha512, DigestAlgorithm::Sha512)
    ) && a.encoded == b.encoded
}

fn reference(tag: Option<&str>, digest: Option<Digest>) -> Reference {
    Reference {
        registry: "registry.example".to_string(),
        repository: "componentized/oci".to_string(),
        tag: tag.map(str::to_string),
        digest,
    }
}

async fn get_manifest(
    registry: impl FnMut(&test_harness::RegistryRequest) -> RegistryResponse + Send + 'static,
    reference: Reference,
) -> wasmtime::Result<(
    Result<Manifest, ErrorCode>,
    Vec<test_harness::RegistryRequest>,
)> {
    let mut client = Harness::new().registry(registry).build().await?;
    let result = client
        .run(async move |accessor, client| client.call_get_manifest(accessor, reference).await)
        .await?;
    Ok((result, client.requests()))
}

#[tokio::test(flavor = "multi_thread")]
async fn gets_manifest_by_digest() -> wasmtime::Result<()> {
    let digest = sha256(MANIFEST);
    let (result, requests) = get_manifest(
        |_| RegistryResponse::ok(MANIFEST),
        reference(None, Some(digest.clone())),
    )
    .await?;
    assert!(matches!(result, Ok(Manifest::OciImageV1(_))), "{result:?}");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        format!(
            "https://registry.example/v2/componentized/oci/manifests/sha256:{}",
            digest.encoded
        )
    );
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "Accept"
                && value.contains("application/vnd.oci.image.manifest.v1+json")),
        "{:?}",
        requests[0].headers
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_manifest_not_matching_digest() -> wasmtime::Result<()> {
    let tampered = String::from_utf8(MANIFEST.to_vec())
        .unwrap()
        .replace("1894585", "1894586");
    let (result, _) = get_manifest(
        move |_| RegistryResponse::ok(tampered.clone()),
        reference(None, Some(sha256(MANIFEST))),
    )
    .await?;
    match result {
        Err(ErrorCode::DigestInvalid(message)) => {
            assert!(message.contains("digest mismatch"), "{message}")
        }
        other => panic!("expected digest-invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gets_manifest_by_tag_without_verifying() -> wasmtime::Result<()> {
    let (result, requests) = get_manifest(
        |_| RegistryResponse::ok(MANIFEST),
        reference(Some("v1"), None),
    )
    .await?;
    assert!(matches!(result, Ok(Manifest::OciImageV1(_))), "{result:?}");
    assert_eq!(
        requests[0].url,
        "https://registry.example/v2/componentized/oci/manifests/v1"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_registry_error_for_manifest() -> wasmtime::Result<()> {
    let (result, _) = get_manifest(
        |_| RegistryResponse::error(404, "MANIFEST_UNKNOWN", "manifest unknown"),
        reference(Some("v1"), None),
    )
    .await?;
    assert!(
        matches!(&result, Err(ErrorCode::ManifestUnknown(message)) if message == "manifest unknown"),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_digest_of_tagged_manifest() -> wasmtime::Result<()> {
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::ok(MANIFEST))
        .build()
        .await?;
    let result = client
        .run(async move |accessor, client| {
            client
                .call_resolve_digest(accessor, reference(Some("v1"), None))
                .await
        })
        .await?;
    assert!(
        matches!(&result, Ok(digest) if same_digest(digest, &sha256(MANIFEST))),
        "{result:?}"
    );
    let requests = client.requests();
    assert_eq!(
        requests[0].url,
        "https://registry.example/v2/componentized/oci/manifests/v1"
    );
    // the same manifest get-manifest gets for the tag
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "Accept"
                && value.contains("application/vnd.oci.image.manifest.v1+json")),
        "{:?}",
        requests[0].headers
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolves_digested_reference_without_request() -> wasmtime::Result<()> {
    let digest = sha256(MANIFEST);
    let mut client = Harness::new().build().await?;
    let result = client
        .run({
            let digest = digest.clone();
            async move |accessor, client| {
                client
                    .call_resolve_digest(accessor, reference(Some("v1"), Some(digest)))
                    .await
            }
        })
        .await?;
    assert!(
        matches!(&result, Ok(resolved) if same_digest(resolved, &digest)),
        "{result:?}"
    );
    assert_eq!(client.requests(), vec![]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_status_of_unspecified_registry_error() -> wasmtime::Result<()> {
    let (result, _) = get_manifest(
        |_| RegistryResponse {
            status: 500,
            headers: vec![],
            body: br#"{"errors":[]}"#.to_vec(),
        },
        reference(Some("v1"), None),
    )
    .await?;
    assert!(
        matches!(&result, Err(ErrorCode::Other(Some(message))) if message == "transport error with http status 500: unspecified errors"),
        "{result:?}"
    );
    Ok(())
}
