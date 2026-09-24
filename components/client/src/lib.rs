#![cfg_attr(not(test), no_main)]

use std::{collections::BTreeMap, fmt::Display, time::UNIX_EPOCH};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use wit_bindgen::StreamReader;

use crate::{
    componentized::http::client::{self as http, HttpResponse},
    exports::componentized::oci::client::{
        Config, Digest, ErrorCode, Guest, Instant, Manifest,
        MediaType::{self, Other},
        MediaTypeSuffix, OciDescriptorV1, OciImageConfigV1, OciImageConfigV1Config,
        OciImageConfigV1ContentAddresses, OciImageConfigV1HistoryEntry, OciImageIndexManifestV1,
        OciImageIndexManifestV1Manifest, OciImageIndexManifestV1ManifestPlatform,
        OciImageManifestV1, Reference, RetryAfter, SchemaVersion, WasmConfigV0,
        WasmConfigV0Component,
    },
};

pub(crate) struct OCIClient;

impl Guest for OCIClient {
    #[allow(async_fn_in_trait)]
    fn parse_reference(reference: String) -> Result<Reference, ErrorCode> {
        // reference parsing logic is derived from https://pkg.go.dev/github.com/google/go-containerregistry/pkg/name#ParseReference
        let tag_result = Self::tag_reference(reference.clone());
        if tag_result.is_ok() {
            return tag_result;
        }
        let digest_result = Self::digest_reference(reference.clone());
        if digest_result.is_ok() {
            return digest_result;
        }
        Err(ErrorCode::Other(Some(format!(
            "could not parse reference: {reference}"
        ))))
    }

    #[allow(async_fn_in_trait)]
    async fn resolve_digest(reference: Reference) -> Result<Digest, ErrorCode> {
        let Reference {
            registry,
            repository,
            tag,
            digest,
        } = reference;

        if let Some(digest) = digest {
            return Ok(digest);
        }

        let tag = tag.unwrap_or("latest".to_string());
        let url = format!("https://{registry}/v2/{repository}/manifests/{tag}");
        let http::HttpResponse {
            status,
            headers,
            body,
            ..
        } = Self::get(url, vec![]).await?;

        match status {
            200 => Self::compute_digest("sha256", &body.collect().await),
            _ => Err(Self::decode_transport_error(body, status, headers).await),
        }
    }

    #[allow(async_fn_in_trait)]
    async fn get_blob(reference: Reference) -> Result<Vec<u8>, ErrorCode> {
        let Reference {
            registry,
            repository,
            tag: _,
            digest,
        } = reference;
        let digest = match digest {
            Some(digest) => digest,
            None => Err(ErrorCode::BlobUnknown("digest required".to_string()))?,
        };
        let url = format!("https://{registry}/v2/{repository}/blobs/{digest}");
        let http::HttpResponse {
            status,
            headers,
            body,
            ..
        } = Self::get(url, vec![]).await?;

        let raw = match status {
            200 => Ok(body.collect().await),
            _ => Err(Self::decode_transport_error(body, status, headers).await),
        }?;

        Self::assert_digest(digest, &raw)?;

        Ok(raw)
    }

    #[allow(async_fn_in_trait)]
    async fn get_config(
        reference: Reference,
        default_media_type: Option<MediaType>,
    ) -> Result<Config, ErrorCode> {
        let blob = Self::get_blob(reference).await?;

        Self::required(
            Self::parse_config(
                &serde_json::from_slice(&blob)?,
                "$config",
                default_media_type,
            ),
            "$config",
        )
    }

    #[allow(async_fn_in_trait)]
    async fn get_manifest(reference: Reference) -> Result<Manifest, ErrorCode> {
        let Reference {
            registry,
            repository,
            tag,
            digest,
        } = reference;
        let version = match digest.clone() {
            Some(digest) => digest.to_string(),
            None => match tag {
                Some(tag) => tag,
                None => "latest".to_string(),
            },
        };
        let url = format!("https://{registry}/v2/{repository}/manifests/{version}");
        let http::HttpResponse {
            status,
            headers,
            body,
            ..
        } = Self::get(url, vec![
            ("Accept".to_string(), "application/vnd.oci.image.manifest.v1+json, application/vnd.oci.image.index.v1+json".to_string())
        ]).await?;

        let raw = match status {
            200 => body.collect().await,
            _ => Err(Self::decode_transport_error(body, status, headers).await)?,
        };
        if let Some(digest) = digest {
            // check if manifest was requested by digest, ignore if requested by tag
            Self::assert_digest(digest, &raw)?
        }
        Self::required(
            Self::parse_manifest(&serde_json::from_slice(&raw)?, "$manifest"),
            "$manifest",
        )
    }
}

impl OCIClient {
    async fn get(
        url: String,
        mut headers: Vec<(String, String)>,
    ) -> Result<HttpResponse, ErrorCode> {
        let response = Self::get_with_redirects(url.clone(), headers.clone()).await?;
        if response.status != 401 {
            return Ok(response);
        }

        let challenge = response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("www-authenticate"))
            .and_then(|(_, v)| Self::parse_www_authenticate(v));
        let Some((scheme, params)) = challenge else {
            return Ok(response);
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            // other schemes (e.g. basic) require credentials, which are not supported yet
            return Ok(response);
        }

        // https://distribution.github.io/distribution/spec/auth/token/
        let token = Self::fetch_token(&params).await?;
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));

        // a second 401 is returned to the caller and decoded as a transport error
        Self::get_with_redirects(url, headers).await
    }

    /// Issues a GET request, following redirects (e.g. registries redirecting
    /// blob downloads to a CDN). The `Authorization` header is dropped when a
    /// redirect leaves the original origin, as pre-signed storage URLs reject
    /// unexpected credentials and the token must not leak to other hosts.
    async fn get_with_redirects(
        url: String,
        mut headers: Vec<(String, String)>,
    ) -> Result<HttpResponse, ErrorCode> {
        const MAX_REDIRECTS: usize = 10;

        let mut url = Url::parse(&url)
            .map_err(|e| ErrorCode::Other(Some(format!("invalid url {url}: {e}"))))?;
        for _ in 0..=MAX_REDIRECTS {
            let response = http::get(url.to_string(), headers.clone(), None).await?;
            if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                .map(|(_, v)| v.clone())
            else {
                return Ok(response);
            };

            let next = url.join(&location).map_err(|e| {
                ErrorCode::Other(Some(format!("invalid redirect location {location}: {e}")))
            })?;
            if next.origin() != url.origin() {
                headers.retain(|(k, _)| !k.eq_ignore_ascii_case("authorization"));
            }
            url = next;
        }

        Err(ErrorCode::Other(Some(format!(
            "too many redirects, stopped at {url}"
        ))))
    }

    async fn fetch_token(params: &BTreeMap<String, String>) -> Result<String, ErrorCode> {
        let realm = params.get("realm").ok_or_else(|| {
            ErrorCode::Unauthorized("bearer challenge is missing realm".to_string())
        })?;
        let mut url = Url::parse(realm)
            .map_err(|e| ErrorCode::Unauthorized(format!("invalid token realm {realm}: {e}")))?;
        {
            let mut query = url.query_pairs_mut();
            for key in ["service", "scope"] {
                if let Some(value) = params.get(key) {
                    query.append_pair(key, value);
                }
            }
        }

        let http::HttpResponse { status, body, .. } =
            Self::get_with_redirects(url.to_string(), vec![]).await?;
        let body = body.collect().await;
        if status != 200 {
            return Err(ErrorCode::Unauthorized(format!(
                "token request to {realm} failed with status {status}"
            )));
        }

        let TokenResponse {
            token,
            access_token,
        } = serde_json::from_slice(&body)?;
        token.or(access_token).ok_or_else(|| {
            ErrorCode::Unauthorized(format!("token response from {realm} is missing token"))
        })
    }

    /// Parses a single `WWW-Authenticate` challenge into its scheme and
    /// lower-cased auth-params, e.g.
    /// `Bearer realm="https://auth.example/token",service="example",scope="repository:foo:pull"`
    fn parse_www_authenticate(value: &str) -> Option<(String, BTreeMap<String, String>)> {
        let value = value.trim();
        let (scheme, rest) = value.split_once(char::is_whitespace).unwrap_or((value, ""));
        if scheme.is_empty() {
            return None;
        }

        let mut params = BTreeMap::new();
        let mut chars = rest.chars().peekable();
        loop {
            while chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
            if chars.peek().is_none() {
                break;
            }

            let mut key = String::new();
            while let Some(c) = chars.next_if(|c| *c != '=' && *c != ',') {
                key.push(c);
            }
            if chars.next_if_eq(&'=').is_none() {
                // token68 or malformed param, not used by registries
                continue;
            }
            while chars.next_if(|c| c.is_whitespace()).is_some() {}

            let mut val = String::new();
            if chars.next_if_eq(&'"').is_some() {
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => val.extend(chars.next()),
                        '"' => break,
                        _ => val.push(c),
                    }
                }
            } else {
                while let Some(c) = chars.next_if(|c| *c != ',') {
                    val.push(c);
                }
            }
            params.insert(key.trim().to_ascii_lowercase(), val.trim().to_string());
        }

        Some((scheme.to_string(), params))
    }

    fn tag_reference(reference: String) -> Result<Reference, ErrorCode> {
        let mut base = reference.clone();
        let mut tag = String::from("");

        // Split on ":"
        let parts: Vec<&str> = reference.split(':').collect();
        // Verify that we aren't confusing a tag for a hostname w/ port for the purposes of weak validation.
        if parts.len() > 1 && !parts.last().unwrap().contains("/") {
            base = parts[..parts.len() - 1].join(":");
            tag = parts.last().unwrap().to_string();
            if tag == "" {
                return Err(ErrorCode::Other(Some(format!(
                    "{reference} must specify a tag name after the colon"
                ))));
            }
        }

        let mut reference = Self::parse_repository_reference(base)?;
        if tag != "" {
            let re = Regex::new(r"^[a-zA-Z0-9_\-.]{1,128}$").unwrap();
            if !re.is_match(&tag) {
                Err(ErrorCode::Other(Some(format!("invalid tag format: {tag}"))))?
            }
            reference.tag = Some(tag);
        }
        Ok(reference)
    }

    fn digest_reference(reference: String) -> Result<Reference, ErrorCode> {
        // Split on "@"
        let parts: Vec<&str> = reference.split('@').collect();
        if parts.len() != 2 {
            return Err(ErrorCode::Other(Some(format!(
                "a digest must contain exactly one '@' separator (e.g. registry/repository@digest) saw: {reference}"
            ))));
        }
        let base = parts.get(0).unwrap().to_string();
        let digest = parts.get(1).unwrap().to_string();

        let mut reference = Self::tag_reference(base)?;
        reference.digest = Some(Self::digest(digest)?);
        Ok(reference)
    }

    fn digest(reference: String) -> Result<Digest, ErrorCode> {
        let parts: Vec<&str> = reference.split(':').collect();
        if parts.len() != 2 {
            return Err(ErrorCode::Other(Some(String::from(
                "invalid checksum digest format",
            ))));
        }
        let algorithm = parts.get(0).unwrap().to_string();
        let encoded = parts.get(1).unwrap().to_string();

        match algorithm.as_str() {
            "sha256" => {
                let re = Regex::new(r"^[a-f0-9]{64}$").unwrap();
                if !re.is_match(&encoded) {
                    Err(ErrorCode::Other(Some(format!(
                        "invalid checksum digest format: {encoded}"
                    ))))?
                }
            }
            "sha512" => {
                let re = Regex::new(r"^[a-f0-9]{128}$").unwrap();
                if !re.is_match(&encoded) {
                    Err(ErrorCode::Other(Some(format!(
                        "invalid checksum digest format: {encoded}"
                    ))))?
                }
            }
            _ => Err(ErrorCode::Other(Some(format!(
                "unsupported digest algorithm: {algorithm}"
            ))))?,
        }

        Ok(Digest { algorithm, encoded })
    }

    fn compute_digest(algorithm: &str, bytes: &[u8]) -> Result<Digest, ErrorCode> {
        fn hash_hex<D: sha2::Digest>(bytes: &[u8]) -> String {
            let mut hasher = D::new();
            hasher.update(bytes);
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }

        let encoded = match algorithm {
            "sha256" => hash_hex::<sha2::Sha256>(bytes),
            "sha512" => hash_hex::<sha2::Sha512>(bytes),
            _ => Err(ErrorCode::DigestInvalid(format!(
                "unsupported algorithm: {}",
                algorithm
            )))?,
        };

        Ok(Digest {
            algorithm: algorithm.to_string(),
            encoded,
        })
    }

    fn assert_digest(expected_digest: Digest, bytes: &[u8]) -> Result<(), ErrorCode> {
        let actual_digest = Self::compute_digest(&expected_digest.algorithm, bytes)?;
        if expected_digest != actual_digest {
            Err(ErrorCode::DigestInvalid(format!(
                "digest mismatch: expected = {expected_digest}, actual = {actual_digest}",
            )))?
        }
        Ok(())
    }

    fn parse_repository_reference(reference: String) -> Result<Reference, ErrorCode> {
        if reference.len() == 0 {
            return Err(ErrorCode::Other(Some(
                "a repository name must be specified".to_string(),
            )));
        }

        let mut registry = String::from("");
        let mut repository = reference.clone();
        let parts: Vec<&str> = reference.splitn(2, '/').collect();
        let maybe_registry = parts.first().unwrap().to_string();
        if parts.len() == 2
            && (maybe_registry == "localhost" || maybe_registry.contains(['.', ':']))
        {
            // The first part of the repository is treated as the registry domain
            // if it is localhost or contains a '.' or ':' character, otherwise it
            // is all repository and the domain defaults to Docker Hub.
            registry = maybe_registry;
            repository = parts.get(1).unwrap().to_string();
        }

        if registry.len() == 0 || registry == "docker.io" {
            registry = String::from("index.docker.io");
        }

        if registry == "index.docker.io" && !repository.is_empty() && !repository.contains('/') {
            // Official Docker Hub images live under the "library" namespace.
            repository = format!("library/{repository}");
        }

        if !Regex::new(r"^[a-z0-9_\-./]{1,255}$")
            .unwrap()
            .is_match(&repository)
            || repository.starts_with('/')
            || repository.ends_with('/')
        {
            Err(ErrorCode::Other(Some(format!(
                "invalid repository format: {repository}"
            ))))?
        }

        // parse as a url to validate the registry is a valid hostname or ip, the scheme is used to make the parser happy, but is ultimately ignored
        let url = Url::parse(&format!("http://{registry}"));
        let is_valid_registry = url.as_ref().is_ok_and(|url| {
            url.host_str().is_some()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none()
        });
        if !is_valid_registry {
            Err(ErrorCode::Other(Some(format!(
                "invalid registry format: {registry}"
            ))))?
        }

        Ok(Reference {
            registry,
            repository,
            tag: None,
            digest: None,
        })
    }

    async fn decode_transport_error(
        body: StreamReader<u8>,
        status: u16,
        headers: Vec<(String, String)>,
    ) -> ErrorCode {
        if status == 429 {
            let after = headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Retry-After"))
                .map(|(_, v)| {
                    if let Ok(seconds) = v.parse::<u32>() {
                        return Some(RetryAfter::DelaySeconds(seconds));
                    }
                    if let Ok(date) = httpdate::parse_http_date(v) {
                        let seconds = date
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .try_into()
                            .unwrap();
                        if seconds == 0 {
                            return None;
                        }
                        return Some(RetryAfter::Date(Instant {
                            seconds: seconds,
                            nanoseconds: 0,
                        }));
                    }
                    None
                })
                .flatten();
            return ErrorCode::Toomanyrequests(after);
        }

        let body = body.collect().await;

        let transport_errors: serde_json::Result<TransportErrors> = serde_json::from_slice(&body);
        if transport_errors.is_err() {
            return ErrorCode::Other(Some(format!(
                "transport error with http status {status}: {}",
                transport_errors.unwrap_err()
            )));
        }
        let transport_errors = transport_errors.unwrap();
        if transport_errors.errors.len() == 0 {
            return ErrorCode::Other(Some(
                "transport error with http status {status}: unspecified errors".to_string(),
            ));
        }
        let error = transport_errors.errors.get(0).unwrap();

        match error.code.as_str() {
            "BLOB_UNKNOWN" => ErrorCode::BlobUnknown(error.message.to_string()),
            "BLOB_UPLOAD_INVALID" => ErrorCode::BlobUploadInvalid(error.message.to_string()),
            "BLOB_UPLOAD_UNKNOWN" => ErrorCode::BlobUploadUnknown(error.message.to_string()),
            "DIGEST_INVALID" => ErrorCode::DigestInvalid(error.message.to_string()),
            "MANIFEST_BLOB_UNKNOWN" => ErrorCode::ManifestBlobUnknown(error.message.to_string()),
            "MANIFEST_INVALID" => ErrorCode::ManifestInvalid(error.message.to_string()),
            "MANIFEST_UNKNOWN" => ErrorCode::ManifestUnknown(error.message.to_string()),
            "NAME_INVALID" => ErrorCode::NameInvalid(error.message.to_string()),
            "NAME_UNKNOWN" => ErrorCode::NameUnknown(error.message.to_string()),
            "SIZE_INVALID" => ErrorCode::SizeInvalid(error.message.to_string()),
            "UNAUTHORIZED" => ErrorCode::Unauthorized(error.message.to_string()),
            "DENIED" => ErrorCode::Denied(error.message.to_string()),
            "UNSUPPORTED" => ErrorCode::Unsupported(error.message.to_string()),
            _ => ErrorCode::Other(Some(error.message.to_string())),
        }
    }

    fn normalize_media_type(media_type: &str) -> MediaType {
        match media_type {
            "application/vnd.oci.descriptor.v1+json" => {
                MediaType::ApplicationVndOciDescriptorV1(MediaTypeSuffix::Json)
            }
            "application/vnd.oci.layout.header.v1+json" => {
                MediaType::ApplicationVndOciLayoutHeaderV1(MediaTypeSuffix::Json)
            }
            "application/vnd.oci.image.index.v1+json" => {
                MediaType::ApplicationVndOciImageIndexV1(MediaTypeSuffix::Json)
            }
            "application/vnd.oci.image.manifest.v1+json" => {
                MediaType::ApplicationVndOciImageManifestV1(MediaTypeSuffix::Json)
            }
            "application/vnd.oci.image.config.v1+json`" => {
                MediaType::ApplicationVndOciImageConfigV1(MediaTypeSuffix::Json)
            }
            "application/vnd.oci.image.layer.v1.tar" => {
                MediaType::ApplicationVndOciImageLayerV1Tar(MediaTypeSuffix::Other(None))
            }
            "application/vnd.oci.image.layer.v1.tar+gzip" => {
                MediaType::ApplicationVndOciImageLayerV1Tar(MediaTypeSuffix::Gzip)
            }
            "application/vnd.oci.image.layer.v1.tar+zstd" => {
                MediaType::ApplicationVndOciImageLayerV1Tar(MediaTypeSuffix::Zstd)
            }
            "application/vnd.oci.image.layer.nondistributable.v1.tar" => {
                MediaType::ApplicationVndOciImageLayerNondistributableV1Tar(MediaTypeSuffix::Other(
                    None,
                ))
            }
            "application/vnd.oci.image.layer.nondistributable.v1.tar+gzip" => {
                MediaType::ApplicationVndOciImageLayerNondistributableV1Tar(MediaTypeSuffix::Gzip)
            }
            "application/vnd.oci.image.layer.nondistributable.v1.tar+zstd" => {
                MediaType::ApplicationVndOciImageLayerNondistributableV1Tar(MediaTypeSuffix::Zstd)
            }
            "application/vnd.oci.empty.v1+json" => {
                MediaType::ApplicationVndOciEmptyV1(MediaTypeSuffix::Json)
            }
            "application/vnd.wasm.config.v0+json" => {
                MediaType::ApplicationVndWasmConfigV0(MediaTypeSuffix::Json)
            }
            "application/wasm" => MediaType::ApplicationWasm,
            _ => Other(media_type.to_owned()),
        }
    }

    fn parse_manifest(v: &Value, field: &str) -> Result<Option<Manifest>, ErrorCode> {
        let media_type = Self::required(
            Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType")),
            "mediaType",
        )?;
        match media_type {
            MediaType::ApplicationVndOciImageIndexV1(MediaTypeSuffix::Json) => {
                Self::parse_oci_image_index_v1(&v, field)
            }
            MediaType::ApplicationVndOciImageManifestV1(MediaTypeSuffix::Json) => {
                Self::parse_oci_image_manifest_v1(&v, field)
            }
            _ => Ok(Some(Manifest::Other(serde_json::to_vec(v)?))),
        }
    }

    fn parse_oci_image_index_v1(v: &Value, field: &str) -> Result<Option<Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Manifest::OciImageIndexV1(OciImageIndexManifestV1 {
                schema_version: Self::required(
                    Self::parse_schema_version(
                        &v["schemaVersion"],
                        &format!("{field}.schemaVersion"),
                    ),
                    "schemaVersion",
                )?,
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType")),
                    "mediaType",
                )?,
                artifact_type: Self::parse_media_type(
                    &v["artifactType"],
                    &format!("{field}.artifactType"),
                )?,
                manifests: Self::required(
                    Self::parse_list(
                        &v["manifests"],
                        &format!("{field}.manifests"),
                        |v, field| {
                            Self::required(Self::parse_oci_image_index_v1_manifest(v, field), field)
                        },
                    ),
                    "manifest",
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], &format!("{field}.subject"))?,
                annotations: Self::parse_string_map(
                    &v["annotations"],
                    &format!("{field}.annotations"),
                )?,
            }))
        })
    }

    fn parse_oci_image_index_v1_manifest(
        v: &Value,
        field: &str,
    ) -> Result<Option<OciImageIndexManifestV1Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, field| {
            Ok(OciImageIndexManifestV1Manifest {
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType")),
                    "mediaType",
                )?,
                platform: Self::parse_oci_image_index_v1_manifest_platform(
                    &v["platform"],
                    &format!("{field}.platform"),
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], &format!("{field}.subject"))?,
                annotations: Self::parse_string_map(
                    &v["annotations"],
                    &format!("{field}.annotations"),
                )?,
                artifact_type: Self::parse_media_type(
                    &v["artifactType"],
                    &format!("{field}.artifactType"),
                )?,
                data: Self::parse_string(&v["data"], &format!("{field}.data"))?,
                digest: Self::required(
                    Self::parse_digest(&v["digest"], &format!("{field}.digest")),
                    &format!("{field}.digest"),
                )?,
                size: Self::required(
                    Self::parse_u64(&v["size"], &format!("{field}.size")),
                    &format!("{field}.size"),
                )?,
                urls: Self::parse_string_list(&v["urls"], &format!("{field}.urls"))?,
            })
        })
    }

    fn parse_oci_image_index_v1_manifest_platform(
        v: &Value,
        field: &str,
    ) -> Result<Option<OciImageIndexManifestV1ManifestPlatform>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(OciImageIndexManifestV1ManifestPlatform {
                architecture: Self::required(
                    Self::parse_string(&v["architecture"], &format!("{field}.architecture")),
                    "architecture",
                )?,
                os: Self::required(
                    Self::parse_string(&v["os"], &format!("{field}.os")),
                    &format!("{field}.os"),
                )?,
                os_version: Self::parse_string(&v["os.version"], &format!("{field}.os.version"))?,
                os_features: Self::parse_string_list(
                    &v["os.features"],
                    &format!("{field}.os.features"),
                )?,
                variant: Self::parse_string(&v["variant"], &format!("{field}.variant"))?,
            })
        })
    }

    fn parse_oci_descriptor_v1(
        v: &Value,
        field: &str,
    ) -> Result<Option<OciDescriptorV1>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(OciDescriptorV1 {
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType")),
                    "mediaType",
                )?,
                digest: Self::required(
                    Self::parse_digest(&v["digest"], &format!("{field}.digest")),
                    &format!("{field}.digest"),
                )?,
                size: Self::required(
                    Self::parse_u64(&v["size"], &format!("{field}.size")),
                    &format!("{field}.size"),
                )?,
                urls: Self::parse_list(&v["urls"], &format!("{field}.urls"), |v, field| {
                    Self::required(Self::parse_string(v, field), field)
                })?,
                annotations: Self::parse_string_map(
                    &v["annotations"],
                    &format!("{field}.annotations"),
                )?,
                artifact_type: Self::parse_media_type(
                    &v["artifactType"],
                    &format!("{field}.artifactType"),
                )?,
                data: Self::parse_string(&v["data"], &format!("{field}.data"))?,
            })
        })
    }

    fn parse_oci_image_manifest_v1(v: &Value, field: &str) -> Result<Option<Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Manifest::OciImageV1(OciImageManifestV1 {
                schema_version: Self::required(
                    Self::parse_schema_version(
                        &v["schemaVersion"],
                        &format!("{field}.schemaVersion"),
                    ),
                    &format!("{field}.schemaVersion"),
                )?,
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType")),
                    &format!("{field}.mediaType"),
                )?,
                artifact_type: Self::parse_media_type(
                    &v["artifactType"],
                    &format!("{field}.artifactType"),
                )?,
                config: Self::required(
                    Self::parse_oci_descriptor_v1(&v["config"], &format!("{field}.config")),
                    &format!("{field}.config"),
                )?,
                layers: Self::required(
                    Self::parse_list(&v["layers"], &format!("{field}.layers"), |v, field| {
                        Self::required(Self::parse_oci_descriptor_v1(v, field), field)
                    }),
                    &format!("{field}.layers"),
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], &format!("{field}.subject"))?,
                annotations: Self::parse_string_map(
                    &v["annotations"],
                    &format!("{field}.annotations"),
                )?,
            }))
        })
    }

    fn parse_config(
        v: &Value,
        field: &str,
        default_media_type: Option<MediaType>,
    ) -> Result<Option<Config>, ErrorCode> {
        let media_type = Self::required(
            Self::parse_media_type(&v["mediaType"], &format!("{field}.mediaType"))
                .map(|mt| mt.or(default_media_type)),
            &format!("{field}.mediaType"),
        )?;
        match media_type {
            MediaType::ApplicationVndOciImageConfigV1(MediaTypeSuffix::Json) => {
                Self::parse_oci_image_config_v1(&v, field)
            }
            MediaType::ApplicationVndWasmConfigV0(MediaTypeSuffix::Json) => {
                Self::parse_wasm_config_v0(&v, field)
            }
            _ => Ok(Some(Config::Other(serde_json::to_vec(v)?))),
        }
    }

    fn parse_oci_image_config_v1(v: &Value, field: &str) -> Result<Option<Config>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Config::OciImageV1(OciImageConfigV1 {
                created: Self::parse_instant(&v["created"], &format!("{field}.crated"))?,
                author: Self::parse_string(&v["author"], &format!("{field}.author"))?,
                architecture: Self::required(
                    Self::parse_string(&v["architecture"], &format!("{field}.architecture")),
                    &format!("{field}.architecture"),
                )?,
                os: Self::required(
                    Self::parse_string(&v["os"], &format!("{field}.os")),
                    &format!("{field}.os"),
                )?,
                os_version: Self::parse_string(&v["os.version"], &format!("{field}.os.version"))?,
                os_features: Self::parse_string_list(
                    &v["os.features"],
                    &format!("{field}.os.features"),
                )?,
                variant: Self::parse_string(&v["variant"], &format!("{field}.variant"))?,
                config: Self::parse_object(
                    &v["config"],
                    &format!("{field}.config"),
                    |v, field| {
                        Ok(OciImageConfigV1Config {
                            user: Self::parse_string(&v["User"], &format!("{field}.User"))?,
                            exposed_ports: Self::parse_string_set(
                                &v["ExposedPorts"],
                                &format!("{field}.ExposedPorts"),
                            )?,
                            env: Self::parse_string_list(&v["Env"], &format!("{field}.Env"))?,
                            entrypoint: Self::parse_string_list(
                                &v["Entrypoint"],
                                &format!("{field}.Entrypoint"),
                            )?,
                            cmd: Self::parse_string_list(&v["Cmd"], &format!("{field}.Cmd"))?,
                            volumes: Self::parse_string_set(
                                &v["Volumes"],
                                &format!("{field}.Volumes"),
                            )?,
                            working_dir: Self::parse_string(
                                &v["WorkingDir"],
                                &format!("{field}.WorkingDir"),
                            )?,
                            labels: Self::parse_string_map(
                                &v["Labels"],
                                &format!("{field}.Labels"),
                            )?,
                            stop_signal: Self::parse_string(
                                &v["StopSignal"],
                                &format!("{field}.StopSignal"),
                            )?,
                            args_escaped: Self::parse_bool(
                                &v["ArgsEscaped"],
                                &format!("{field}.ArgsEscaped"),
                            )?,
                        })
                    },
                )?,
                rootfs: Self::required(
                    Self::parse_object(&v["rootfs"], &format!("{field}.rootfs"), |v, field| {
                        Ok(OciImageConfigV1ContentAddresses {
                            type_: Self::required(
                                Self::parse_string(&v["type"], &format!("{field}.type")),
                                &format!("{field}.type"),
                            )?,
                            diff_ids: Self::required(
                                Self::parse_list(
                                    &v["diff_ids"],
                                    &format!("{field}.diff_ids"),
                                    |v, field| Self::required(Self::parse_digest(v, field), field),
                                ),
                                &format!("{field}.diff_ids"),
                            )?,
                        })
                    }),
                    &format!("{field}.rootfs"),
                )?,
                history: Self::parse_list(
                    &v["history"],
                    &format!("{field}.history"),
                    |v, field| {
                        Self::required(
                            Self::parse_object(v, field, |v, field| {
                                Ok(OciImageConfigV1HistoryEntry {
                                    created: Self::parse_instant(
                                        &v["created"],
                                        &format!("{field}.created"),
                                    )?,
                                    author: Self::parse_string(
                                        &v["author"],
                                        &format!("{field}.author"),
                                    )?,
                                    created_by: Self::parse_string(
                                        &v["created_by"],
                                        &format!("{field}.created_by"),
                                    )?,
                                    comment: Self::parse_string(
                                        &v["comment"],
                                        &format!("{field}.comment"),
                                    )?,
                                    empty_layer: Self::parse_bool(
                                        &v["empty_layer"],
                                        &format!("{field}.empty_layer"),
                                    )?,
                                })
                            }),
                            field,
                        )
                    },
                )?,
            }))
        })
    }

    fn parse_wasm_config_v0(v: &Value, field: &str) -> Result<Option<Config>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Config::WasmV0(WasmConfigV0 {
                created: Self::parse_instant(&v["created"], &format!("{field}.created"))?,
                author: Self::parse_string(&v["author"], &format!("{field}.author"))?,
                architecture: Self::required(
                    Self::parse_string(&v["architecture"], &format!("{field}.architecture")),
                    &format!("{field}.architecture"),
                )?,
                os: Self::required(
                    Self::parse_string(&v["os"], &format!("{field}.os")),
                    &format!("{field}.os"),
                )?,
                layer_digests: Self::required(
                    Self::parse_list(
                        &v["layerDigests"],
                        &format!("{field}.layerDigests"),
                        |v, field| Self::required(Self::parse_digest(v, field), field),
                    ),
                    &format!("{field}.layerDigests"),
                )?,
                component: Self::parse_object(
                    &v["component"],
                    &format!("{field}.component"),
                    |v, field| {
                        Ok(WasmConfigV0Component {
                            exports: Self::parse_string_list(
                                &v["exports"],
                                &format!("{field}.exports"),
                            )?
                            .unwrap_or(vec![]),
                            imports: Self::parse_string_list(
                                &v["imports"],
                                &format!("{field}.imports"),
                            )?
                            .unwrap_or(vec![]),
                            target: Self::parse_string(&v["target"], &format!("{field}.target"))?,
                        })
                    },
                )?,
            }))
        })
    }

    fn parse_bool(v: &Value, field: &str) -> Result<Option<bool>, ErrorCode> {
        match v {
            Value::Bool(v) => Ok(Some(v.clone())),
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected a bool for field: {field}"
            )))),
        }
    }

    fn parse_u64(v: &Value, field: &str) -> Result<Option<u64>, ErrorCode> {
        match v {
            Value::Number(v) => match v.as_u64() {
                Some(v) => Ok(Some(v)),
                None => Err(ErrorCode::Other(Some(format!(
                    "expected an integer for field: {field}"
                )))),
            },
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected a number for field: {field}"
            )))),
        }
    }

    fn parse_u8(v: &Value, field: &str) -> Result<Option<u8>, ErrorCode> {
        match Self::parse_u64(v, field)? {
            Some(v) => match v.try_into() {
                Ok(v) => Ok(Some(v)),
                Err(_) => Err(ErrorCode::Other(Some(format!(
                    "expected an integer for field: {field}"
                )))),
            },
            None => Ok(None),
        }
    }

    fn parse_string(v: &Value, field: &str) -> Result<Option<String>, ErrorCode> {
        match v {
            Value::String(v) => Ok(Some(v.clone())),
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected a string for field: {field}"
            )))),
        }
    }

    fn parse_string_list(v: &Value, field: &str) -> Result<Option<Vec<String>>, ErrorCode> {
        Self::parse_list(v, field, |v, field| {
            Self::required(Self::parse_string(v, field), field)
        })
    }

    fn parse_string_map(
        v: &Value,
        field: &str,
    ) -> Result<Option<BTreeMap<String, String>>, ErrorCode> {
        match v {
            Value::Object(values) => {
                let mut map = BTreeMap::new();
                for (k, v) in values {
                    let field = &format!("{field}['{k}']");
                    map.insert(
                        k.to_string(),
                        Self::required(Self::parse_string(v, field), field)?,
                    );
                }
                Ok(Some(map))
            }
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected an object for field: {field}"
            ))))?,
        }
    }

    fn parse_string_set(v: &Value, field: &str) -> Result<Option<Vec<String>>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            let mut set = vec![];
            if let Value::Object(v) = v {
                for (k, _) in v {
                    set.push(k.to_string());
                }
            }
            Ok(set)
        })
    }

    fn parse_media_type(v: &Value, field: &str) -> Result<Option<MediaType>, ErrorCode> {
        Ok(Self::parse_string(v, field)?.map(|v| Self::normalize_media_type(&v)))
    }

    fn parse_instant(v: &Value, field: &str) -> Result<Option<Instant>, ErrorCode> {
        match Self::parse_string(v, field)? {
            Some(timestamp) => {
                let date_time = chrono::DateTime::parse_from_rfc3339(&timestamp)?;
                Ok(Some(Instant {
                    seconds: date_time.timestamp(),
                    nanoseconds: date_time.timestamp_subsec_nanos(),
                }))
            }
            None => Ok(None),
        }
    }

    fn parse_schema_version(v: &Value, field: &str) -> Result<Option<SchemaVersion>, ErrorCode> {
        match Self::parse_u8(v, field)? {
            Some(version) => match version {
                2 => Ok(Some(SchemaVersion::V2)),
                _ => Ok(Some(SchemaVersion::Other(Some(version as u8)))),
            },
            None => Ok(None),
        }
    }

    fn parse_digest(v: &Value, field: &str) -> Result<Option<Digest>, ErrorCode> {
        match Self::parse_string(v, field)? {
            Some(digest) => Ok(Some(Self::digest(digest)?)),
            None => Ok(None),
        }
    }

    fn required<T>(v: Result<Option<T>, ErrorCode>, field: &str) -> Result<T, ErrorCode> {
        match v? {
            Some(v) => Ok(v),
            None => Err(ErrorCode::Other(Some(format!(
                "missing required field: {field}"
            )))),
        }
    }

    fn parse_list<T>(
        v: &Value,
        field: &str,
        mapper: impl Fn(&Value, &str) -> Result<T, ErrorCode>,
    ) -> Result<Option<Vec<T>>, ErrorCode> {
        match v {
            Value::Array(values) => {
                let mut list = vec![];
                for (i, v) in values.iter().enumerate() {
                    list.push(mapper(v, &format!("{field}[{i}]"))?);
                }
                Ok(Some(list))
            }
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected an array for field: {field}"
            ))))?,
        }
    }

    fn parse_object<T>(
        v: &Value,
        field: &str,
        mapper: impl Fn(&Value, &str) -> Result<T, ErrorCode>,
    ) -> Result<Option<T>, ErrorCode> {
        match v {
            Value::Object(_) => Ok(Some(mapper(v, field)?)),
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected an object for field: {field}"
            ))))?,
        }
    }
}

#[derive(Deserialize, Debug)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Deserialize, Debug)]
struct TransportErrors {
    errors: Vec<TransportError>,
}

#[derive(Deserialize, Debug)]
struct TransportError {
    code: String,
    message: String,
    // detail: Option<String>,
}

impl From<http::ErrorCode> for ErrorCode {
    fn from(value: http::ErrorCode) -> Self {
        match value {
            http::ErrorCode::Other(message) => Self::Other(message),
        }
    }
}

impl From<serde_json::Error> for ErrorCode {
    fn from(value: serde_json::Error) -> Self {
        Self::Other(Some(value.to_string()))
    }
}

impl From<chrono::ParseError> for ErrorCode {
    fn from(value: chrono::ParseError) -> Self {
        Self::Other(Some(value.to_string()))
    }
}

impl Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { algorithm, encoded } = self;
        f.write_fmt(format_args!("{algorithm}:{encoded}"))
    }
}

impl PartialEq for Reference {
    fn eq(&self, other: &Self) -> bool {
        self.registry == other.registry
            && self.repository == other.repository
            && self.tag == other.tag
            && self.digest == other.digest
    }
}

impl PartialEq for Digest {
    fn eq(&self, other: &Self) -> bool {
        self.algorithm == other.algorithm && self.encoded == other.encoded
    }
}

impl PartialEq for ErrorCode {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (ErrorCode::BlobUnknown(this), ErrorCode::BlobUnknown(other)) => this == other,
            (ErrorCode::BlobUploadInvalid(this), ErrorCode::BlobUploadInvalid(other)) => {
                this == other
            }
            (ErrorCode::BlobUploadUnknown(this), ErrorCode::BlobUploadUnknown(other)) => {
                this == other
            }
            (ErrorCode::DigestInvalid(this), ErrorCode::DigestInvalid(other)) => this == other,
            (ErrorCode::ManifestBlobUnknown(this), ErrorCode::ManifestBlobUnknown(other)) => {
                this == other
            }
            (ErrorCode::ManifestInvalid(this), ErrorCode::ManifestInvalid(other)) => this == other,
            (ErrorCode::ManifestUnknown(this), ErrorCode::ManifestUnknown(other)) => this == other,
            (ErrorCode::NameInvalid(this), ErrorCode::NameInvalid(other)) => this == other,
            (ErrorCode::NameUnknown(this), ErrorCode::NameUnknown(other)) => this == other,
            (ErrorCode::SizeInvalid(this), ErrorCode::SizeInvalid(other)) => this == other,
            (ErrorCode::Unauthorized(this), ErrorCode::Unauthorized(other)) => this == other,
            (ErrorCode::Denied(this), ErrorCode::Denied(other)) => this == other,
            (ErrorCode::Unsupported(this), ErrorCode::Unsupported(other)) => this == other,
            (ErrorCode::Toomanyrequests(this), ErrorCode::Toomanyrequests(other)) => this == other,
            (ErrorCode::Other(this), ErrorCode::Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for RetryAfter {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RetryAfter::Date(this), RetryAfter::Date(other)) => this == other,
            (RetryAfter::DelaySeconds(this), RetryAfter::DelaySeconds(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for Instant {
    fn eq(&self, other: &Self) -> bool {
        self.seconds == other.seconds && self.nanoseconds == other.nanoseconds
    }
}

impl PartialEq for MediaType {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                MediaType::ApplicationVndOciDescriptorV1(this),
                MediaType::ApplicationVndOciDescriptorV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciLayoutHeaderV1(this),
                MediaType::ApplicationVndOciLayoutHeaderV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciImageIndexV1(this),
                MediaType::ApplicationVndOciImageIndexV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciImageManifestV1(this),
                MediaType::ApplicationVndOciImageManifestV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciImageConfigV1(this),
                MediaType::ApplicationVndOciImageConfigV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciImageLayerV1Tar(this),
                MediaType::ApplicationVndOciImageLayerV1Tar(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciEmptyV1(this),
                MediaType::ApplicationVndOciEmptyV1(other),
            ) => this == other,
            (
                MediaType::ApplicationVndOciImageLayerNondistributableV1Tar(this),
                MediaType::ApplicationVndOciImageLayerNondistributableV1Tar(other),
            ) => this == other,
            (
                MediaType::ApplicationVndWasmConfigV0(this),
                MediaType::ApplicationVndWasmConfigV0(other),
            ) => this == other,
            (MediaType::ApplicationWasm, MediaType::ApplicationWasm) => true,
            (Other(this), Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for MediaTypeSuffix {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (MediaTypeSuffix::Json, MediaTypeSuffix::Json) => true,
            (MediaTypeSuffix::Gzip, MediaTypeSuffix::Gzip) => true,
            (MediaTypeSuffix::Zstd, MediaTypeSuffix::Zstd) => true,
            (MediaTypeSuffix::Other(this), MediaTypeSuffix::Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for SchemaVersion {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SchemaVersion::V2, SchemaVersion::V2) => true,
            (SchemaVersion::Other(this), SchemaVersion::Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for Manifest {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Manifest::OciImageV1(this), Manifest::OciImageV1(other)) => this == other,
            (Manifest::OciImageIndexV1(this), Manifest::OciImageIndexV1(other)) => this == other,
            (Manifest::Other(this), Manifest::Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for OciImageManifestV1 {
    fn eq(&self, other: &Self) -> bool {
        self.annotations == other.annotations
            && self.artifact_type == other.artifact_type
            && self.config == other.config
            && self.layers == other.layers
            && self.media_type == other.media_type
            && self.schema_version == other.schema_version
            && self.subject == other.subject
    }
}

impl PartialEq for OciImageIndexManifestV1 {
    fn eq(&self, other: &Self) -> bool {
        self.annotations == other.annotations
            && self.artifact_type == other.artifact_type
            && self.manifests == other.manifests
            && self.media_type == other.media_type
            && self.schema_version == other.schema_version
            && self.subject == other.subject
    }
}

impl PartialEq for OciImageIndexManifestV1Manifest {
    fn eq(&self, other: &Self) -> bool {
        self.annotations == other.annotations
            && self.digest == other.digest
            && self.media_type == other.media_type
            && self.size == other.size
            && self.urls == other.urls
            && self.artifact_type == other.artifact_type
            && self.data == other.data
            && self.platform == other.platform
            && self.subject == other.subject
    }
}

impl PartialEq for OciImageIndexManifestV1ManifestPlatform {
    fn eq(&self, other: &Self) -> bool {
        self.architecture == other.architecture
            && self.os == other.os
            && self.os_version == other.os_version
            && self.os_features == other.os_features
            && self.variant == other.variant
    }
}

impl PartialEq for OciDescriptorV1 {
    fn eq(&self, other: &Self) -> bool {
        self.annotations == other.annotations
            && self.digest == other.digest
            && self.media_type == other.media_type
            && self.size == other.size
            && self.urls == other.urls
            && self.artifact_type == other.artifact_type
            && self.data == other.data
    }
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Config::OciImageV1(this), Config::OciImageV1(other)) => this == other,
            (Config::WasmV0(this), Config::WasmV0(other)) => this == other,
            (Config::Other(this), Config::Other(other)) => this == other,
            _ => false,
        }
    }
}

impl PartialEq for OciImageConfigV1 {
    fn eq(&self, other: &Self) -> bool {
        self.created == other.created
            && self.author == other.author
            && self.architecture == other.architecture
            && self.os == other.os
            && self.os_version == other.os_version
            && self.os_features == other.os_features
            && self.variant == other.variant
            && self.config == other.config
            && self.rootfs == other.rootfs
            && self.history == other.history
    }
}

impl PartialEq for OciImageConfigV1Config {
    fn eq(&self, other: &Self) -> bool {
        self.user == other.user
            && self.exposed_ports == other.exposed_ports
            && self.env == other.env
            && self.entrypoint == other.entrypoint
            && self.cmd == other.cmd
            && self.volumes == other.volumes
            && self.working_dir == other.working_dir
            && self.labels == other.labels
            && self.stop_signal == other.stop_signal
            && self.args_escaped == other.args_escaped
    }
}

impl PartialEq for OciImageConfigV1ContentAddresses {
    fn eq(&self, other: &Self) -> bool {
        self.type_ == other.type_ && self.diff_ids == other.diff_ids
    }
}

impl PartialEq for OciImageConfigV1HistoryEntry {
    fn eq(&self, other: &Self) -> bool {
        self.created == other.created
            && self.author == other.author
            && self.created_by == other.created_by
            && self.comment == other.comment
            && self.empty_layer == other.empty_layer
    }
}

impl PartialEq for WasmConfigV0 {
    fn eq(&self, other: &Self) -> bool {
        self.created == other.created
            && self.author == other.author
            && self.architecture == other.architecture
            && self.os == other.os
            && self.layer_digests == other.layer_digests
            && self.component == other.component
    }
}

impl PartialEq for WasmConfigV0Component {
    fn eq(&self, other: &Self) -> bool {
        self.exports == other.exports
            && self.imports == other.imports
            && self.target == other.target
    }
}

wit_bindgen::generate!({
    path: "../wit",
    world: "client",
    merge_structurally_equal_types: true,
    generate_all
});

export!(OCIClient);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_ref(
        registry: &str,
        repository: &str,
        tag: Option<String>,
        digest: Option<Digest>,
    ) -> Result<Reference, ErrorCode> {
        Ok(Reference {
            registry: registry.to_string(),
            repository: repository.to_string(),
            tag: tag,
            digest: digest,
        })
    }

    fn make_tag(tag: &str) -> Option<String> {
        Some(tag.to_string())
    }

    fn make_digest(algorithm: &str, encoded: &str) -> Option<Digest> {
        Some(Digest {
            algorithm: algorithm.to_string(),
            encoded: encoded.to_string(),
        })
    }

    fn make_err(reference: &str) -> Result<Reference, ErrorCode> {
        Err(ErrorCode::Other(Some(format!(
            "could not parse reference: {reference}"
        ))))
    }

    #[test]
    fn test_parse_reference() {
        let sha256 = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let sha512 = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";

        struct TestCase {
            input: &'static str,
            expected: Result<Reference, ErrorCode>,
            description: &'static str,
        }

        let table = vec![
            TestCase {
                input: "foo/bar",
                expected: make_ref("index.docker.io", "foo/bar", None, None),
                description: "namespaced repository defaults to Docker Hub",
            },
            TestCase {
                input: "ubuntu",
                expected: make_ref("index.docker.io", "library/ubuntu", None, None),
                description: "single-segment repository defaults to Docker Hub",
            },
            TestCase {
                input: "docker.io/library/ubuntu",
                expected: make_ref("index.docker.io", "library/ubuntu", None, None),
                description: "docker.io is normalized to index.docker.io",
            },
            TestCase {
                input: "registry.example/foo/bar",
                expected: make_ref("registry.example", "foo/bar", None, None),
                description: "explicit registry with a dot",
            },
            TestCase {
                input: "registry.example:5000/foo/bar",
                expected: make_ref("registry.example:5000", "foo/bar", None, None),
                description: "explicit registry with a dot and a port",
            },
            TestCase {
                input: "localhost/foo/bar",
                expected: make_ref("localhost", "foo/bar", None, None),
                description: "localhost is treated as a registry even without a dot",
            },
            TestCase {
                input: "localhost:5000/foo/bar",
                expected: make_ref("localhost:5000", "foo/bar", None, None),
                description: "localhost with a port is treated as a registry",
            },
            TestCase {
                input: "192.168.1.1/foo/bar",
                expected: make_ref("192.168.1.1", "foo/bar", None, None),
                description: "IP literal registry",
            },
            TestCase {
                input: "192.168.1.500/foo/bar",
                expected: make_err("192.168.1.500/foo/bar"),
                description: "invalid IP literal registry",
            },
            TestCase {
                input: "[2001:0db8:85a3:0000:0000:8a2e:0370:7334]/foo/bar",
                expected: make_ref(
                    "[2001:0db8:85a3:0000:0000:8a2e:0370:7334]",
                    "foo/bar",
                    None,
                    None,
                ),
                description: "IPv6 literal registry",
            },
            TestCase {
                input: "[2001:db8::1]/foo/bar",
                expected: make_ref("[2001:db8::1]", "foo/bar", None, None),
                description: "IPv6 compressed literal registry",
            },
            TestCase {
                input: "[2001:db8::1]:5000/foo/bar",
                expected: make_ref("[2001:db8::1]:5000", "foo/bar", None, None),
                description: "IPv6 literal registry with port",
            },
            TestCase {
                input: "[2001:db8::1]:5000/foo/bar:v1",
                expected: make_ref("[2001:db8::1]:5000", "foo/bar", make_tag("v1"), None),
                description: "IPv6 literal registry with port and tag",
            },
            TestCase {
                input: "[2001:db8:1111:2222:3333:4444:5555:6666:7777]/foo/bar",
                expected: make_err("[2001:db8:1111:2222:3333:4444:5555:6666:7777]/foo/bar"),
                description: "invalid IPv6 literal registry",
            },
            TestCase {
                input: "",
                expected: make_err(""),
                description: "empty reference is rejected",
            },
            TestCase {
                input: "/foo/bar",
                expected: make_err("/foo/bar"),
                description: "repository cannot start with a slash",
            },
            TestCase {
                input: "foo/bar/",
                expected: make_err("foo/bar/"),
                description: "repository cannot end with a slash",
            },
            TestCase {
                input: "Foo/Bar",
                expected: make_err("Foo/Bar"),
                description: "repository must be lowercase",
            },
            TestCase {
                input: "foo/bar:v1",
                expected: make_ref("index.docker.io", "foo/bar", make_tag("v1"), None),
                description: "simple tag",
            },
            TestCase {
                input: "foo/bar:1.0.0-alpha_1",
                expected: make_ref(
                    "index.docker.io",
                    "foo/bar",
                    make_tag("1.0.0-alpha_1"),
                    None,
                ),
                description: "tag with dots, dashes and underscores",
            },
            TestCase {
                input: "localhost:5000/foo/bar:v1",
                expected: make_ref("localhost:5000", "foo/bar", make_tag("v1"), None),
                description: "tag alongside a registry with a port is not confused with the port",
            },
            TestCase {
                input: "foo/bar:",
                expected: make_err("foo/bar:"),
                description: "empty tag after the colon is rejected",
            },
            TestCase {
                input: "foo/bar:inva!id",
                expected: make_err("foo/bar:inva!id"),
                description: "tag with an invalid character is rejected",
            },
            TestCase {
                input: "foo/bar@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                expected: make_ref(
                    "index.docker.io",
                    "foo/bar",
                    None,
                    make_digest("sha256", sha256),
                ),
                description: "valid sha256 digest",
            },
            TestCase {
                input: "foo/bar@sha512:cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
                expected: make_ref(
                    "index.docker.io",
                    "foo/bar",
                    None,
                    make_digest("sha512", sha512),
                ),
                description: "valid sha512 digest",
            },
            TestCase {
                input: "foo/bar:v1@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                expected: make_ref(
                    "index.docker.io",
                    "foo/bar",
                    make_tag("v1"),
                    make_digest("sha256", sha256),
                ),
                description: "valid tag and digest",
            },
            TestCase {
                input: "foo/bar@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
                expected: make_err(
                    "foo/bar@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85",
                ),
                description: "sha256 digest with the wrong length is rejected",
            },
            TestCase {
                input: "foo/bar@md5:abcdef",
                expected: make_err("foo/bar@md5:abcdef"),
                description: "unsupported digest algorithm is rejected",
            },
            TestCase {
                input: "foo/bar@sha256:shortdigest",
                expected: make_err("foo/bar@sha256:shortdigest"),
                description: "malformed digest encoding is rejected",
            },
            TestCase {
                input: "foo/bar@",
                expected: make_err("foo/bar@"),
                description: "missing digest after the '@' is rejected",
            },
            TestCase {
                input: "foo/bar@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85@extra",
                expected: make_err(
                    "foo/bar@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b85@extra",
                ),
                description: "more than one '@' separator is rejected",
            },
            TestCase {
                input: "foo/bar:v1@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                expected: make_ref(
                    "index.docker.io",
                    "foo/bar",
                    make_tag("v1"),
                    make_digest("sha256", sha256),
                ),
                description: "when both a tag and a digest are present, the digest wins and the tag is dropped",
            },
        ];

        for case in table {
            assert_eq!(
                OCIClient::parse_reference(case.input.to_string()),
                case.expected,
                "Failed assertion on case: {}",
                case.description
            );
        }
    }

    #[test]
    fn test_parse_www_authenticate() {
        let params = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };

        assert_eq!(
            OCIClient::parse_www_authenticate(
                r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull,push""#
            ),
            Some((
                "Bearer".to_string(),
                params(&[
                    ("realm", "https://auth.docker.io/token"),
                    ("service", "registry.docker.io"),
                    ("scope", "repository:library/alpine:pull,push"),
                ])
            )),
        );
        assert_eq!(
            OCIClient::parse_www_authenticate(
                r#"Basic Realm = "a \"quoted\" realm" , charset=UTF-8"#
            ),
            Some((
                "Basic".to_string(),
                params(&[("realm", r#"a "quoted" realm"#), ("charset", "UTF-8")])
            )),
        );
        assert_eq!(
            OCIClient::parse_www_authenticate("Bearer"),
            Some(("Bearer".to_string(), params(&[]))),
        );
        assert_eq!(OCIClient::parse_www_authenticate("  "), None);
    }

    #[test]
    fn test_parse_manifest() {
        assert_eq!(
            OCIClient::parse_manifest(&json!({
                "annotations":{
                    "org.opencontainers.image.description": "OCI image client component.",
                    "org.opencontainers.image.licenses": "Apache-2.0",
                    "org.opencontainers.image.revision": "23f1b1de0b55b4d3a1d0b417f0c2492263323e2e",
                    "org.opencontainers.image.source": "https://github.com/componentized/oci.git",
                    "org.opencontainers.image.title": "client",
                    "org.opencontainers.image.version": "0.0.0-dev"
                },
                "config": {
                    "digest": "sha256:80d83bbdaa82cff96584c99217c29fd17bc7e5f0424c0cb26aa3831ea18132b4",
                    "mediaType": "application/vnd.wasm.config.v0+json",
                    "size": 345
                },
                "layers": [
                    {
                        "annotations": {
                            "org.opencontainers.image.title": "client.wasm"
                        },
                        "digest": "sha256:ee7ff5c9588e997b4a54b6d351b52a5ca4f6980377f59e48c778f48a23b483db",
                        "mediaType": "application/wasm",
                        "size": 1894585
                    }
                ],
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "schemaVersion": 2
            }), "").unwrap(),
            Some(Manifest::OciImageV1(OciImageManifestV1 {
                annotations: Some(BTreeMap::from([
                    ("org.opencontainers.image.description".to_string(), "OCI image client component.".to_string()),
                    ("org.opencontainers.image.licenses".to_string(), "Apache-2.0".to_string()),
                    ("org.opencontainers.image.revision".to_string(), "23f1b1de0b55b4d3a1d0b417f0c2492263323e2e".to_string()),
                    ("org.opencontainers.image.source".to_string(), "https://github.com/componentized/oci.git".to_string()),
                    ("org.opencontainers.image.title".to_string(), "client".to_string()),
                    ("org.opencontainers.image.version".to_string(), "0.0.0-dev".to_string()),
                ])),
                artifact_type: None,
                config: OciDescriptorV1 {
                    annotations: None,
                    digest: Digest { algorithm: "sha256".to_string(), encoded: "80d83bbdaa82cff96584c99217c29fd17bc7e5f0424c0cb26aa3831ea18132b4".to_string() },
                    media_type: MediaType::ApplicationVndWasmConfigV0(MediaTypeSuffix::Json),
                    size: 345,
                    urls: None,
                    artifact_type: None,
                    data: None,
                },
                layers: vec![OciDescriptorV1{
                    annotations: Some(BTreeMap::from([
                        ("org.opencontainers.image.title".to_string(), "client.wasm".to_string()),
                    ])),
                    digest: Digest { algorithm: "sha256".to_string(), encoded: "ee7ff5c9588e997b4a54b6d351b52a5ca4f6980377f59e48c778f48a23b483db".to_string() },
                    media_type: MediaType::ApplicationWasm,
                    size: 1894585,
                    urls: None,
                    artifact_type: None,
                    data: None,
                 }],
                media_type: MediaType::ApplicationVndOciImageManifestV1(MediaTypeSuffix::Json),
                schema_version: SchemaVersion::V2,
                subject: None,
            })),
        );

        assert_eq!(
            OCIClient::parse_manifest(&json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [
                    {
                        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                        "size": 743,
                        "digest": "sha256:9434033b4008b51c0c9270dda9315ea4229901fee28a7980085091e9fd4b62b8",
                        "platform": {
                            "architecture": "amd64",
                            "os": "linux"
                        },
                        "artifactType": "application/vnd.docker.container.image.v1+json"
                    },
                    {
                        "mediaType": "application/vnd.docker.distribution.manifest.v2+json",
                        "size": 743,
                        "digest": "sha256:eac7a2bcae76b2bc5b5fed23033ffba56283462e315c2035b6ab5b2c8c80bd34",
                        "platform": {
                            "architecture": "arm64",
                            "os": "linux"
                        },
                        "artifactType": "application/vnd.docker.container.image.v1+json"
                    }
                ]
            }), "").unwrap(),
            Some(Manifest::OciImageIndexV1(OciImageIndexManifestV1 {
                schema_version: SchemaVersion::V2,
                media_type: MediaType::ApplicationVndOciImageIndexV1(MediaTypeSuffix::Json),
                artifact_type: None,
                manifests: vec![
                    OciImageIndexManifestV1Manifest{
                        annotations:None,
                        media_type:MediaType::Other("application/vnd.docker.distribution.manifest.v2+json".to_string()),
                        size:743,
                        platform:Some(OciImageIndexManifestV1ManifestPlatform{
                            architecture:"amd64".to_string(),
                            os:"linux".to_string(),
                            os_features:None,
                            os_version:None,
                            variant:None,
                        }),
                        subject:None,
                        digest: Digest { algorithm: "sha256".to_string(), encoded: "9434033b4008b51c0c9270dda9315ea4229901fee28a7980085091e9fd4b62b8".to_string() },
                        urls: None,
                        data: None,
                        artifact_type: Some(MediaType::Other("application/vnd.docker.container.image.v1+json".to_string()))
                    },
                    OciImageIndexManifestV1Manifest{
                        annotations:None,
                        media_type:MediaType::Other("application/vnd.docker.distribution.manifest.v2+json".to_string()),
                        size:743,
                        platform:Some(OciImageIndexManifestV1ManifestPlatform{
                            architecture:"arm64".to_string(),
                            os:"linux".to_string(),
                            os_features:None,os_version:None,variant:None,
                        }),
                        subject:None,
                        digest: Digest { algorithm: "sha256".to_string(), encoded: "eac7a2bcae76b2bc5b5fed23033ffba56283462e315c2035b6ab5b2c8c80bd34".to_string() },
                        urls: None,
                        data: None,
                        artifact_type: Some(MediaType::Other("application/vnd.docker.container.image.v1+json".to_string())),
                    },
                ],
                subject: None,
                annotations: None,
            }))
        );
    }

    #[test]
    fn test_parse_config() {
        assert_eq!(
            OCIClient::parse_config(&json!({
                "architecture": "amd64",
                "author": "github.com/ko-build/ko",
                "created": "2026-07-08T23:06:13Z",
                "history":[
                    {
                        "author": "apko",
                        "created": "2026-07-08T23:06:13Z",
                        "created_by": "apko",
                        "comment": "static by Chainguard"
                    },
                    {
                        "author": "ko",
                        "created": "0001-01-01T00:00:00Z",
                        "created_by": "ko build ko://github.com/servicebinding/runtime",
                        "comment": "kodata contents, at $KO_DATA_PATH"
                    },
                    {
                        "author": "ko",
                        "created": "0001-01-01T00:00:00Z",
                        "created_by": "ko build ko://github.com/servicebinding/runtime",
                        "comment": "go build output, at /ko-app/runtime"
                    }
                ],
                "os": "linux",
                "rootfs":{
                    "type": "layers",
                    "diff_ids":[
                        "sha256:458136df58646e7146e8240b685e4e6bfffa019ba10d3c221ff59b3928f54d8c",
                        "sha256:ffe56a1c5f3878e9b5f803842adb9e2ce81584b6bd027e8599582aefe14a975b",
                        "sha256:38217aaa0c148dcb7f3af90a384d30ccb4a7736a9f15b75a5d6a9f28c586964b"
                    ]
                },
                "config":{
                    "Entrypoint":["/ko-app/runtime"],
                    "Env":[
                        "PATH=/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/sbin:/bin:/ko-app",
                        "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt",
                        "KO_DATA_PATH=/var/run/ko"
                    ],
                    "Labels":{
                        "dev.chainguard.image.title": "static",
                        "dev.chainguard.package.main": "",
                        "org.opencontainers.image.authors": "Chainguard Team https://www.chainguard.dev/",
                        "org.opencontainers.image.created": "2026-07-08T23:06:13Z",
                        "org.opencontainers.image.source": "https://github.com/chainguard-images/images/tree/main/images/static",
                        "org.opencontainers.image.title": "static",
                        "org.opencontainers.image.url": "https://images.chainguard.dev/directory/image/static/overview",
                        "org.opencontainers.image.vendor": "Chainguard"
                    },
                    "User": "65532"
                }
            }), "", Some(MediaType::ApplicationVndOciImageConfigV1(MediaTypeSuffix::Json))).unwrap(),
            Some(Config::OciImageV1(OciImageConfigV1 {
                architecture: "amd64".to_string(),
                author: Some("github.com/ko-build/ko".to_string()),
                created: Some(Instant{seconds:1783551973, nanoseconds:0}), // "2026-07-08T23:06:13Z"
                history: Some(vec![
                    OciImageConfigV1HistoryEntry{
                        author: Some("apko".to_string()),
                        created: Some(Instant{seconds:1783551973, nanoseconds:0}), // "2026-07-08T23:06:13Z"
                        created_by: Some("apko".to_string()),
                        comment: Some("static by Chainguard".to_string()),
                        empty_layer: None,
                    },
                    OciImageConfigV1HistoryEntry{
                        author: Some("ko".to_string()),
                        created: Some(Instant{seconds:-62135596800, nanoseconds:0}), // "0001-01-01T00:00:00Z"
                        created_by: Some("ko build ko://github.com/servicebinding/runtime".to_string()),
                        comment:Some( "kodata contents, at $KO_DATA_PATH".to_string()),
                        empty_layer: None,
                    },
                    OciImageConfigV1HistoryEntry{
                        author: Some("ko".to_string()),
                        created: Some(Instant{seconds:-62135596800, nanoseconds:0}), // "0001-01-01T00:00:00Z"
                        created_by: Some("ko build ko://github.com/servicebinding/runtime".to_string()),
                        comment: Some("go build output, at /ko-app/runtime".to_string()),
                        empty_layer: None,
                    }
                ]),
                os: "linux".to_string(),
                os_features: None,
                os_version: None,
                variant: None,
                rootfs: OciImageConfigV1ContentAddresses {
                    type_: "layers".to_string(),
                    diff_ids: vec![
                        Digest{algorithm:"sha256".to_string(), encoded:"458136df58646e7146e8240b685e4e6bfffa019ba10d3c221ff59b3928f54d8c".to_string()},
                        Digest{algorithm:"sha256".to_string(), encoded:"ffe56a1c5f3878e9b5f803842adb9e2ce81584b6bd027e8599582aefe14a975b".to_string()},
                        Digest{algorithm:"sha256".to_string(), encoded:"38217aaa0c148dcb7f3af90a384d30ccb4a7736a9f15b75a5d6a9f28c586964b".to_string()},
                    ]
                },
                config: Some(OciImageConfigV1Config{
                    entrypoint: Some(vec!["/ko-app/runtime".to_string()]),
                    env:Some(vec![
                        "PATH=/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/sbin:/bin:/ko-app".to_string(),
                        "SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt".to_string(),
                        "KO_DATA_PATH=/var/run/ko".to_string()
                    ]),
                    labels: Some(BTreeMap::from([
                        ("dev.chainguard.image.title".to_string(), "static".to_string()),
                        ("dev.chainguard.package.main".to_string(), "".to_string()),
                        ("org.opencontainers.image.authors".to_string(), "Chainguard Team https://www.chainguard.dev/".to_string()),
                        ("org.opencontainers.image.created".to_string(), "2026-07-08T23:06:13Z".to_string()),
                        ("org.opencontainers.image.source".to_string(), "https://github.com/chainguard-images/images/tree/main/images/static".to_string()),
                        ("org.opencontainers.image.title".to_string(), "static".to_string()),
                        ("org.opencontainers.image.url".to_string(), "https://images.chainguard.dev/directory/image/static/overview".to_string()),
                        ("org.opencontainers.image.vendor".to_string(), "Chainguard".to_string())
                    ])),
                    user: Some("65532".to_string()),
                    args_escaped: None,
                    cmd: None,
                    exposed_ports: None,
                    stop_signal: None,
                    volumes: None,
                    working_dir: None,
                })
            })),
        );

        assert_eq!(
            OCIClient::parse_config(
                &json!({
                    "created": "2026-09-22T14:58:42.504222730Z",
                    "author": null,
                    "architecture": "wasm",
                    "os": "wasip2",
                    "layerDigests": [
                        "sha256:ee7ff5c9588e997b4a54b6d351b52a5ca4f6980377f59e48c778f48a23b483db"
                    ],
                    "component": {
                        "exports": [
                            "componentized:oci/client@0.0.0-dev"
                        ],
                        "imports":[
                            "componentized:http/client@0.1.0-dev",
                            "wasi:clocks/system-clock@0.3.0"
                        ],
                        "target": null
                    }
                }),
                "",
                Some(MediaType::ApplicationVndWasmConfigV0(MediaTypeSuffix::Json))
            )
            .unwrap(),
            Some(Config::WasmV0(WasmConfigV0 {
                created: Some(Instant {
                    seconds: 1790089122,
                    nanoseconds: 504222730,
                }),
                author: None,
                architecture: "wasm".to_string(),
                os: "wasip2".to_string(),
                layer_digests: vec![Digest {
                    algorithm: "sha256".to_string(),
                    encoded: "ee7ff5c9588e997b4a54b6d351b52a5ca4f6980377f59e48c778f48a23b483db"
                        .to_string()
                }],
                component: Some(WasmConfigV0Component {
                    exports: vec!["componentized:oci/client@0.0.0-dev".to_string(),],
                    imports: vec![
                        "componentized:http/client@0.1.0-dev".to_string(),
                        "wasi:clocks/system-clock@0.3.0".to_string()
                    ],
                    target: None,
                }),
            })),
        );
    }
}
