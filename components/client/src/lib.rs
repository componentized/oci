#![cfg_attr(not(test), no_main)]

use std::{collections::BTreeMap, fmt::Display, time::UNIX_EPOCH};

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use url::Url;
use wit_bindgen::StreamReader;

use crate::{
    componentized::http::client as http,
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
        } = http::get(url, vec![], None).await?;

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
        } = http::get(url, vec![], None).await?;

        let raw = match status {
            200 => Ok(body.collect().await),
            _ => Err(Self::decode_transport_error(body, status, headers).await),
        }?;

        Self::assert_digest(digest, &raw)?;

        Ok(raw)
    }

    #[allow(async_fn_in_trait)]
    async fn get_config(reference: Reference) -> Result<Config, ErrorCode> {
        let blob = Self::get_blob(reference).await?;
        let parsed: Value = serde_json::from_slice(&blob)?;

        let media_type = Self::required(
            Self::parse_media_type(&parsed["mediaType"], "mediaType"),
            "mediaType",
        )?;
        match media_type {
            MediaType::ApplicationVndOciImageConfigV1(MediaTypeSuffix::Json) => {
                Self::required(Self::parse_oci_image_config_v1(&parsed, ""), "")
            }
            MediaType::ApplicationVndWasmConfigV0(MediaTypeSuffix::Json) => {
                Self::required(Self::parse_wasm_config_v0(&parsed, ""), "")
            }
            _ => Ok(Config::Other(blob)),
        }
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
        } = http::get(url, vec![], None).await?;

        let raw = match status {
            200 => body.collect().await,
            _ => Err(Self::decode_transport_error(body, status, headers).await)?,
        };
        if let Some(digest) = digest {
            // check if manifest was requested by digest, ignore if requested by tag
            Self::assert_digest(digest, &raw)?
        }
        let parsed: Value = serde_json::from_slice(&raw)?;

        let media_type = Self::required(
            Self::parse_media_type(&parsed["mediaType"], "mediaType"),
            "mediaType",
        )?;
        match media_type {
            MediaType::ApplicationVndOciImageIndexV1(MediaTypeSuffix::Json) => {
                Self::required(Self::parse_oci_image_index_v1(&parsed, ""), "")
            }
            MediaType::ApplicationVndOciImageManifestV1(MediaTypeSuffix::Json) => {
                Self::required(Self::parse_oci_image_manifest_v1(&parsed, ""), "")
            }
            _ => Ok(Manifest::Other(raw)),
        }
    }
}

impl OCIClient {
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
            return Err(ErrorCode::Other(Some(format!("a digest must contain exactly one '@' separator (e.g. registry/repository@digest) saw: {reference}"))));
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
            return ErrorCode::Other(Some("unknown transport error".to_string()));
        }
        let transport_errors = transport_errors.unwrap();
        if transport_errors.errors.len() == 0 {
            return ErrorCode::Other(Some("unknown transport error".to_string()));
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

    fn parse_oci_image_index_v1(v: &Value, field: &str) -> Result<Option<Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Manifest::OciImageIndexV1(OciImageIndexManifestV1 {
                schema_version: Self::required(
                    Self::parse_schema_version(&v["schemaVersion"], "schemaVersion"),
                    "schemaVersion",
                )?,
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], "mediaType"),
                    "mediaType",
                )?,
                artifact_type: Self::parse_media_type(&v["artifactType"], "artifactType")?,
                manifests: Self::required(
                    Self::parse_list(&v["manifests"], "manifests", |v, field| {
                        Self::required(Self::parse_oci_image_index_v1_manifest(v, field), field)
                    }),
                    "manifest",
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], "subject")?,
                annotations: Self::parse_string_map(&v["annotations"], "annotations")?,
            }))
        })
    }

    fn parse_oci_image_index_v1_manifest(
        v: &Value,
        field: &str,
    ) -> Result<Option<OciImageIndexManifestV1Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(OciImageIndexManifestV1Manifest {
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], "mediaType"),
                    "mediaType",
                )?,
                platform: Self::parse_oci_image_index_v1_manifest_platform(
                    &v["platform"],
                    "platform",
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], "subject")?,
                annotations: Self::parse_string_map(&v["annotations"], "annotations")?,
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
                    Self::parse_string(&v["architecture"], "architecture"),
                    "architecture",
                )?,
                os: Self::required(Self::parse_string(&v["os"], "os"), "os")?,
                os_version: Self::parse_string(&v["os.version"], "os.version")?,
                os_features: Self::parse_string_list(&v["os.features"], "os.features")?,
                variant: Self::parse_string(&v["variant"], "variant")?,
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
                    Self::parse_media_type(&v["mediaType"], "mediaType"),
                    "mediaType",
                )?,
                digest: Self::required(Self::parse_digest(&v["digest"], "digest"), "digest")?,
                size: Self::required(Self::parse_u64(&v["size"], "size"), "size")?,
                urls: Self::parse_list(&v["urls"], "urls", |v, field| {
                    Self::required(Self::parse_string(v, field), field)
                })?,
                annotations: Self::parse_string_map(&v["annotations"], "annotations")?,
            })
        })
    }

    fn parse_oci_image_manifest_v1(v: &Value, field: &str) -> Result<Option<Manifest>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Manifest::OciImageV1(OciImageManifestV1 {
                schema_version: Self::required(
                    Self::parse_schema_version(&v["schemaVersion"], "schemaVersion"),
                    "schemaVersion",
                )?,
                media_type: Self::required(
                    Self::parse_media_type(&v["mediaType"], "mediaType"),
                    "mediaType",
                )?,
                artifact_type: Self::parse_media_type(&v["artifactType"], "artifactType")?,
                config: Self::required(
                    Self::parse_oci_descriptor_v1(&v["config"], "config"),
                    "config",
                )?,
                layers: Self::required(
                    Self::parse_list(&v["layers"], "layers", |v, field| {
                        Self::required(Self::parse_oci_descriptor_v1(v, field), field)
                    }),
                    "layers",
                )?,
                subject: Self::parse_oci_descriptor_v1(&v["subject"], "subject")?,
                annotations: Self::parse_string_map(&v["annotations"], "annotations")?,
            }))
        })
    }

    fn parse_oci_image_config_v1(v: &Value, field: &str) -> Result<Option<Config>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Config::OciImageV1(OciImageConfigV1 {
                created: Self::parse_instant(&v["created"], "crated")?,
                author: Self::parse_string(&v["author"], "author")?,
                architecture: Self::required(
                    Self::parse_string(&v["architecture"], "architecture"),
                    "architecture",
                )?,
                os: Self::required(Self::parse_string(&v["os"], "os"), "os")?,
                os_version: Self::parse_string(&v["os.version"], "os.version")?,
                os_features: Self::parse_string_list(&v["os.features"], "os.features")?,
                variant: Self::parse_string(&v["variant"], "variant")?,
                config: match &v["config"] {
                    Value::Object(v) => Some(OciImageConfigV1Config {
                        user: Self::parse_string(&v["User"], "User")?,
                        exposed_ports: Self::parse_string_set(&v["ExposedPorts"], "ExposedPorts")?,
                        env: Self::parse_string_list(&v["Env"], "Env")?,
                        entrypoint: Self::parse_string_list(&v["Entrypoint"], "Entrypoint")?,
                        cmd: Self::parse_string_list(&v["Cmd"], "Cmd")?,
                        volumes: Self::parse_string_set(&v["Volumes"], "Volumes")?,
                        working_dir: Self::parse_string(&v["WorkingDir"], "WorkingDir")?,
                        labels: Self::parse_string_map(&v["Labels"], "Labels")?,
                        stop_signal: Self::parse_string(&v["StopSignal"], "StopSignal")?,
                        args_escaped: Self::parse_bool(&v["ArgsEscaped"], "ArgsEscaped")?,
                    }),
                    Value::Null => None,
                    _ => Err(ErrorCode::Other(Some(
                        "expected an object for field: config".to_string(),
                    )))?,
                },
                rootfs: match &v["rootfs"] {
                    Value::Object(v) => OciImageConfigV1ContentAddresses {
                        type_: Self::required(Self::parse_string(&v["type"], "type"), "type")?,
                        diff_ids: match &v["diff_ids"] {
                            Value::Array(values) => {
                                let mut diff_ids = vec![];
                                for (i, v) in values.iter().enumerate() {
                                    let field = &format!("diff_ids[{i}]");
                                    diff_ids
                                        .push(Self::required(Self::parse_string(v, field), field)?);
                                }
                                diff_ids
                            }
                            Value::Null => Err(ErrorCode::Other(Some(
                                "missing required field: diff_ids".to_string(),
                            )))?,
                            _ => Err(ErrorCode::Other(Some(
                                "unexpected type for field: diff_ids".to_string(),
                            )))?,
                        },
                    },
                    Value::Null => Err(ErrorCode::Other(Some(
                        "missing required field: rootfs".to_string(),
                    )))?,
                    _ => Err(ErrorCode::Other(Some(
                        "unknown config type, expected object".to_string(),
                    )))?,
                },
                history: Self::parse_list(&v["history"], "history", |v, field| {
                    Self::required(
                        match v {
                            Value::Object(v) => Ok(Some(OciImageConfigV1HistoryEntry {
                                created: Self::parse_instant(&v["created"], "created")?,
                                author: Self::parse_string(&v["author"], "author")?,
                                created_by: Self::parse_string(&v["created_by"], "created_by")?,
                                comment: Self::parse_string(&v["comment"], "comment")?,
                                empty_layer: Self::parse_bool(&v["empty_layer"], "empty_layer")?,
                            })),
                            Value::Null => Ok(None),
                            _ => Err(ErrorCode::Other(Some(
                                "unknown item in history array, expected object".to_string(),
                            ))),
                        },
                        field,
                    )
                })?,
            }))
        })
    }

    fn parse_wasm_config_v0(v: &Value, field: &str) -> Result<Option<Config>, ErrorCode> {
        Self::parse_object(v, field, |v, _field| {
            Ok(Config::WasmV0(WasmConfigV0 {
                created: Self::parse_instant(&v["created"], "created")?,
                author: Self::parse_string(&v["author"], "author")?,
                architecture: Self::required(
                    Self::parse_string(&v["architecture"], "architecture"),
                    "architecture",
                )?,
                os: Self::required(Self::parse_string(&v["os"], "os"), "os")?,
                layer_digests: match &v["layerDigests"] {
                    Value::Array(values) => {
                        let mut digests = vec![];
                        for (i, v) in values.iter().enumerate() {
                            let field = &format!("layerDigests[{i}]");
                            digests.push(Self::digest(Self::required(
                                Self::parse_string(v, field),
                                field,
                            )?)?);
                        }
                        digests
                    }
                    Value::Null => Err(ErrorCode::Other(Some(
                        "missing required field: layerDigests".to_string(),
                    )))?,
                    _ => Err(ErrorCode::Other(Some(
                        "unknown layerDigests type, expected array".to_string(),
                    )))?,
                },
                component: match &v["component"] {
                    Value::Object(component) => Some(WasmConfigV0Component {
                        exports: match &component["exports"] {
                            Value::Array(values) => {
                                let mut exports = vec![];
                                for (i, v) in values.iter().enumerate() {
                                    let field = &format!("exports[{i}])");
                                    exports
                                        .push(Self::required(Self::parse_string(v, field), field)?);
                                }
                                exports
                            }
                            Value::Null => vec![],
                            _ => Err(ErrorCode::Other(Some(
                                "unknown exports type, expected array".to_string(),
                            )))?,
                        },
                        imports: match &component["imports"] {
                            Value::Array(values) => {
                                let mut imports = vec![];
                                for (i, v) in values.iter().enumerate() {
                                    let field = &format!("imports[{i}])");
                                    imports
                                        .push(Self::required(Self::parse_string(v, field), field)?);
                                }
                                imports
                            }
                            Value::Null => vec![],
                            _ => Err(ErrorCode::Other(Some(
                                "unknown imports type, expected array".to_string(),
                            )))?,
                        },
                        target: Self::parse_string(&v["target"], "target")?,
                    }),
                    Value::Null => None,
                    _ => Err(ErrorCode::Other(Some(
                        "unknown component type, expected object".to_string(),
                    )))?,
                },
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
            for (k, _) in v {
                set.push(k.to_string());
            }
            Ok(set)
        })
    }

    fn parse_media_type(v: &Value, field: &str) -> Result<Option<MediaType>, ErrorCode> {
        let media_type = Self::required(Self::parse_string(v, field), field)?;
        Ok(Some(Self::normalize_media_type(&media_type)))
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
        mapper: impl Fn(&serde_json::Map<String, Value>, &str) -> Result<T, ErrorCode>,
    ) -> Result<Option<T>, ErrorCode> {
        match v {
            Value::Object(v) => Ok(Some(mapper(v, field)?)),
            Value::Null => Ok(None),
            _ => Err(ErrorCode::Other(Some(format!(
                "expected an object for field: {field}"
            ))))?,
        }
    }
}

#[derive(Deserialize, Debug)]
struct TransportErrors {
    errors: Vec<TransportError>,
}

#[derive(Deserialize, Debug)]
struct TransportError {
    code: String,
    message: String,
    _detail: String,
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
        self.registry == other.registry && self.repository == other.repository
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
                expected: make_ref("[2001:0db8:85a3:0000:0000:8a2e:0370:7334]", "foo/bar", None, None),
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
                expected: make_err(
                    "[2001:db8:1111:2222:3333:4444:5555:6666:7777]/foo/bar",
                ),
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
                expected: make_ref("index.docker.io", "foo/bar", make_tag("1.0.0-alpha_1"), None),
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
                expected: make_ref("index.docker.io", "foo/bar", None, make_digest("sha256", sha256)),
                description: "valid sha256 digest",
            },
            TestCase {
                input: "foo/bar@sha512:cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
                expected: make_ref("index.docker.io", "foo/bar", None, make_digest("sha512", sha512)),
                description: "valid sha512 digest",
            },
            TestCase {
                input: "foo/bar:v1@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
                expected: make_ref("index.docker.io", "foo/bar", make_tag("v1"), make_digest("sha256", sha256)),
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
                expected: make_ref("index.docker.io", "foo/bar", None, make_digest("sha256", sha256)),
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
}
