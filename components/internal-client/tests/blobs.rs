//! Tests for the blobs streamed by `client`, against a scripted registry.

use test_harness::{
    Config, Digest, ErrorCode, Harness, Reference, RegistryResponse, read, resolve, sha256,
};

/// Content larger than the client reads from the registry at once, so it streams in chunks.
fn content() -> Vec<u8> {
    (0..1024 * 1024 + 7).map(|i| (i % 251) as u8).collect()
}

fn reference(digest: Option<Digest>) -> Reference {
    Reference {
        registry: "registry.example".to_string(),
        repository: "componentized/oci".to_string(),
        tag: None,
        digest,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_blob() -> wasmtime::Result<()> {
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::ok(content()))
        .build()
        .await?;
    let (bytes, verified) = client
        .run(async move |accessor, client| {
            let (stream, verified) = client
                .call_get_blob(accessor, reference(Some(sha256(&content()))))
                .await?
                .expect("get-blob");
            let bytes = read(accessor, stream, None).await?;
            let verified = resolve(accessor, verified).await?;
            Ok((bytes, verified))
        })
        .await?;
    assert_eq!(bytes, content());
    assert!(matches!(verified, Ok(())), "{verified:?}");
    let requests = client.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        format!(
            "https://registry.example/v2/componentized/oci/blobs/sha256:{}",
            sha256(&content()).encoded
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_blob_not_matching_digest() -> wasmtime::Result<()> {
    let mut tampered = content();
    tampered[512 * 1024] ^= 0xff;
    let mut client = Harness::new()
        .registry(move |_| RegistryResponse::ok(tampered.clone()))
        .build()
        .await?;
    let (bytes, verified) = client
        .run(async move |accessor, client| {
            let (stream, verified) = client
                .call_get_blob(accessor, reference(Some(sha256(&content()))))
                .await?
                .expect("get-blob");
            let bytes = read(accessor, stream, None).await?;
            let verified = resolve(accessor, verified).await?;
            Ok((bytes, verified))
        })
        .await?;
    // the content streams before it can be verified, the future reports the mismatch
    assert_eq!(bytes.len(), content().len());
    match verified {
        Err(ErrorCode::DigestInvalid(message)) => {
            assert!(message.contains("digest mismatch"), "{message}")
        }
        other => panic!("expected digest-invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_truncated_blob() -> wasmtime::Result<()> {
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::ok(content()[..1000].to_vec()))
        .build()
        .await?;
    let verified = client
        .run(async move |accessor, client| {
            let (stream, verified) = client
                .call_get_blob(accessor, reference(Some(sha256(&content()))))
                .await?
                .expect("get-blob");
            read(accessor, stream, None).await?;
            resolve(accessor, verified).await
        })
        .await?;
    assert!(
        matches!(verified, Err(ErrorCode::DigestInvalid(_))),
        "{verified:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn blob_not_verified_when_stream_dropped() -> wasmtime::Result<()> {
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::ok(content()))
        .build()
        .await?;
    let (bytes, verified) = client
        .run(async move |accessor, client| {
            let (stream, verified) = client
                .call_get_blob(accessor, reference(Some(sha256(&content()))))
                .await?
                .expect("get-blob");
            let bytes = read(accessor, stream, Some(1)).await?;
            let verified = resolve(accessor, verified).await?;
            Ok((bytes, verified))
        })
        .await?;
    assert!(bytes.len() < content().len());
    match verified {
        Err(ErrorCode::DigestInvalid(message)) => {
            assert!(message.contains("not verified"), "{message}")
        }
        other => panic!("expected digest-invalid, got {other:?}"),
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn returns_registry_error_before_streaming() -> wasmtime::Result<()> {
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::error(404, "BLOB_UNKNOWN", "blob unknown to registry"))
        .build()
        .await?;
    let result = client
        .run(async move |accessor, client| {
            Ok(client
                .call_get_blob(accessor, reference(Some(sha256(&content()))))
                .await?
                .map(|_| ()))
        })
        .await?;
    assert!(
        matches!(&result, Err(ErrorCode::BlobUnknown(message)) if message == "blob unknown to registry"),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn requires_digest() -> wasmtime::Result<()> {
    let mut client = Harness::new().build().await?;
    let result = client
        .run(async move |accessor, client| {
            Ok(client
                .call_get_blob(accessor, reference(None))
                .await?
                .map(|_| ()))
        })
        .await?;
    assert!(
        matches!(&result, Err(ErrorCode::BlobUnknown(message)) if message == "digest required"),
        "{result:?}"
    );
    assert_eq!(client.requests(), vec![]);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_config_not_matching_digest() -> wasmtime::Result<()> {
    let config = br#"{"architecture":"wasm","os":"wasip2","layerDigests":[]}"#;
    let mut client = Harness::new()
        .registry(|_| {
            RegistryResponse::ok(&br#"{"architecture":"wasm","os":"wasip3","layerDigests":[]}"#[..])
        })
        .build()
        .await?;
    let result = client
        .run(async move |accessor, client| {
            Ok(client
                .call_get_config(accessor, reference(Some(sha256(config))), None)
                .await?
                .map(|_| ()))
        })
        .await?;
    assert!(
        matches!(result, Err(ErrorCode::DigestInvalid(_))),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gets_config() -> wasmtime::Result<()> {
    let config = br#"{"mediaType":"application/vnd.wasm.config.v0+json","architecture":"wasm","os":"wasip2","layerDigests":[]}"#;
    let mut client = Harness::new()
        .registry(|_| RegistryResponse::ok(&config[..]))
        .build()
        .await?;
    let result = client
        .run(async move |accessor, client| {
            Ok(client
                .call_get_config(accessor, reference(Some(sha256(config))), None)
                .await?)
        })
        .await?;
    match result {
        Ok(Config::WasmV0(config)) => assert_eq!(config.os, "wasip2"),
        other => panic!("expected a wasm config, got {other:?}"),
    }
    Ok(())
}
