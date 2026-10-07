//! Test harness for the client component.
//!
//! A [`Harness`] instantiates the client component (`target/components/client/client.wasm`) with a
//! host `componentized:http/client`. Requests the component sends never reach the network, they
//! are recorded and answered by a scripted registry.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::{mpsc, oneshot};
use wasmtime::component::{
    Accessor, Component, FutureConsumer, FutureReader, HasData, Lift, Linker, ResourceTable,
    Source, StreamConsumer, StreamReader, StreamResult,
};
use wasmtime::error::Context as _;
use wasmtime::{Engine, Result, Store, StoreContextMut, bail, format_err};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::bindings::componentized::http::client as http_client;

pub mod bindings {
    wasmtime::component::bindgen!({
        path: "../../components/wit",
        world: "componentized:oci-components/client",
        imports: {
            "componentized:http/client": async | store,
        },
        exports: { default: async | store },
        with: {
            "wasi:clocks": wasmtime_wasi::p3::bindings::clocks,
        },
    });
}

pub use bindings::exports::componentized::oci::client::{
    Config, Digest, DigestAlgorithm, ErrorCode, Guest as Client, Manifest, Reference,
};

/// The sha256 digest of the content.
pub fn sha256(content: &[u8]) -> Digest {
    use sha2::Digest as _;
    Digest {
        algorithm: DigestAlgorithm::Sha256,
        encoded: sha2::Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}

/// Root of the workspace, where the Makefile lives.
fn workspace_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    dir.canonicalize().unwrap_or(dir)
}

/// Rebuild the client component in `target/components/` with make, once for this process.
fn ensure_built() -> Result<PathBuf> {
    static BUILT: Mutex<bool> = Mutex::new(false);

    let target = "target/components/client/client.wasm";
    // hold the lock while building so concurrent tests don't run make over each other
    let mut built = BUILT.lock().unwrap_or_else(|err| err.into_inner());
    if !*built {
        let output = Command::new("make")
            .arg("-C")
            .arg(workspace_dir())
            .arg(target)
            .output()
            .context("failed to run make")?;
        if !output.status.success() {
            bail!(
                "failed to build {target}:\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        *built = true;
    }
    Ok(workspace_dir().join(target))
}

/// A request the client sent to the registry.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// The registry's response to a request.
#[derive(Clone, Debug, PartialEq)]
pub struct RegistryResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RegistryResponse {
    /// A 200 response with the body.
    pub fn ok(body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: body.into(),
        }
    }

    /// An error response with an OCI distribution spec error, e.g. `BLOB_UNKNOWN`.
    pub fn error(status: u16, code: &str, message: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: format!(r#"{{"errors":[{{"code":"{code}","message":"{message}"}}]}}"#)
                .into_bytes(),
        }
    }
}

type Responder = dyn FnMut(&RegistryRequest) -> RegistryResponse + Send;

pub struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
    responder: Box<Responder>,
    requests: Arc<Mutex<Vec<RegistryRequest>>>,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// The host `componentized:http/client`, requests are recorded and answered by the registry.
struct HttpClient;

impl HasData for HttpClient {
    type Data<'a> = &'a mut Ctx;
}

impl http_client::Host for Ctx {}

impl HttpClient {
    fn respond<T: 'static>(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<StreamReader<u8>>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        let err = |err: wasmtime::Error| http_client::ErrorCode::Other(Some(err.to_string()));
        accessor.with(|mut store| {
            if let Some(mut body) = body {
                body.close(&mut store).map_err(err)?;
            }
            let ctx = store.get();
            let request = RegistryRequest { url, headers };
            let response = (ctx.responder)(&request);
            ctx.requests.lock().unwrap().push(request);

            let body = StreamReader::new(&mut store, response.body).map_err(err)?;
            let trailers =
                FutureReader::new(&mut store, async { Ok::<_, wasmtime::Error>(Ok(vec![])) })
                    .map_err(err)?;
            Ok(http_client::HttpResponse {
                status: response.status,
                headers: response.headers,
                body,
                trailers,
            })
        })
    }
}

impl<T: 'static> http_client::HostWithStore<T> for HttpClient {
    async fn request(
        accessor: &Accessor<T, Self>,
        _method: http_client::Method,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<StreamReader<u8>>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, body)
    }

    async fn get(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, None)
    }

    async fn post(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        body: StreamReader<u8>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, Some(body))
    }

    async fn put(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        body: StreamReader<u8>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, Some(body))
    }

    async fn delete(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, None)
    }

    async fn patch(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        body: StreamReader<u8>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, Some(body))
    }

    async fn head(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, None)
    }

    async fn options(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, None)
    }

    async fn trace(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, None)
    }

    async fn query(
        accessor: &Accessor<T, Self>,
        url: String,
        headers: Vec<(String, String)>,
        body: StreamReader<u8>,
        _options: Option<http_client::RequestOptions>,
    ) -> Result<http_client::HttpResponse, http_client::ErrorCode> {
        Self::respond(accessor, url, headers, Some(body))
    }
}

/// Builds a client instance for a test.
pub struct Harness {
    responder: Box<Responder>,
}

impl Harness {
    /// Test the client component, by default every request is answered with a 404.
    pub fn new() -> Self {
        Self {
            responder: Box::new(|_| RegistryResponse::error(404, "NAME_UNKNOWN", "not found")),
        }
    }

    /// Answer the requests the client sends with the responses.
    pub fn registry(
        mut self,
        responder: impl FnMut(&RegistryRequest) -> RegistryResponse + Send + 'static,
    ) -> Self {
        self.responder = Box::new(responder);
        self
    }

    /// Instantiate the client.
    pub async fn build(self) -> Result<TestSubject> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model_async(true);
        // manifests and configs have `map<string, string>` annotations and labels
        config.wasm_component_model_map(true);
        let engine = Engine::new(&config)?;

        let path = ensure_built()?;
        let component = Component::from_file(&engine, &path)
            .with_context(|| format!("failed to load {}", path.display()))?;

        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        http_client::add_to_linker::<_, HttpClient>(&mut linker, |ctx| ctx)?;

        let requests = Arc::new(Mutex::new(vec![]));
        let mut store = Store::new(
            &engine,
            Ctx {
                wasi: WasiCtxBuilder::new().inherit_stdio().build(),
                table: ResourceTable::new(),
                responder: self.responder,
                requests: requests.clone(),
            },
        );
        let client = bindings::Client::instantiate_async(&mut store, &component, &linker)
            .await
            .context("failed to instantiate the client")?;

        Ok(TestSubject {
            store,
            client,
            requests,
        })
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// An instantiated client.
pub struct TestSubject {
    store: Store<Ctx>,
    client: bindings::Client,
    requests: Arc<Mutex<Vec<RegistryRequest>>>,
}

impl TestSubject {
    /// The requests the client sent to the registry.
    pub fn requests(&self) -> Vec<RegistryRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Run a test body against the client's exported `componentized:oci/client`.
    pub async fn run<R: Send + 'static>(
        &mut self,
        f: impl AsyncFnOnce(&Accessor<Ctx>, &Client) -> Result<R> + Send,
    ) -> Result<R> {
        let client = self.client.componentized_oci_client();
        self.store
            .run_concurrent(async move |accessor| f(accessor, client).await)
            .await?
    }
}

/// Read the content of a guest byte stream, until it closes, or until at least `limit` bytes are
/// read, then the stream is dropped.
pub async fn read(
    accessor: &Accessor<Ctx>,
    stream: StreamReader<u8>,
    limit: Option<usize>,
) -> Result<Vec<u8>> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    accessor.with(|store| {
        stream.pipe(
            store,
            BytesConsumer {
                tx,
                remaining: limit.unwrap_or(usize::MAX),
            },
        )
    })?;
    let mut bytes = vec![];
    while let Some(chunk) = rx.recv().await {
        bytes.extend(chunk);
    }
    Ok(bytes)
}

/// Wait for the value of a guest future.
pub async fn resolve<T: Lift + Send + Sync + 'static>(
    accessor: &Accessor<Ctx>,
    future: FutureReader<T>,
) -> Result<T> {
    let (tx, rx) = oneshot::channel();
    accessor.with(|store| future.pipe(store, OneshotConsumer(Some(tx))))?;
    rx.await
        .map_err(|_| format_err!("future closed without a value"))
}

struct BytesConsumer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    remaining: usize,
}

impl<D> StreamConsumer<D> for BytesConsumer {
    type Item = u8;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        store: StoreContextMut<D>,
        source: Source<'_, u8>,
        _finish: bool,
    ) -> Poll<Result<StreamResult>> {
        let this = self.get_mut();
        let mut source = source.as_direct(store);
        let chunk = source.remaining().to_vec();
        source.mark_read(chunk.len());
        this.remaining = this.remaining.saturating_sub(chunk.len());
        if this.tx.send(chunk).is_err() || this.remaining == 0 {
            return Poll::Ready(Ok(StreamResult::Dropped));
        }
        Poll::Ready(Ok(StreamResult::Completed))
    }
}

struct OneshotConsumer<T>(Option<oneshot::Sender<T>>);

impl<D, T: Lift + Send + Sync + 'static> FutureConsumer<D> for OneshotConsumer<T> {
    type Item = T;

    fn poll_consume(
        self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        store: StoreContextMut<D>,
        mut source: Source<'_, T>,
        _finish: bool,
    ) -> Poll<Result<()>> {
        let mut item = None;
        source.read(store, &mut item)?;
        if let (Some(item), Some(tx)) = (item, self.get_mut().0.take()) {
            // the receiver is only dropped when the test stopped waiting
            let _ = tx.send(item);
        }
        Poll::Ready(Ok(()))
    }
}
