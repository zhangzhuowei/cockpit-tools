//! Authenticated loopback control plane; the Go sidecar retains real upstream I/O.
//! State methods share the native identity/checkpoint implementation.
//! Never log request bodies, headers, tokens, owner IDs or upstream events here.
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Duration,
};

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{watch, Semaphore},
    task::JoinHandle,
};

use super::contract::ServiceError;
use session::Core;

pub mod protocol;
mod session;

#[derive(Clone, Deserialize, Serialize)]
pub struct Config {
    pub identity_path: PathBuf,
    pub token: String,
    pub owners: HashMap<String, String>,
    pub caller_accounts: HashMap<String, Vec<String>>,
}

struct StateData {
    core: Arc<Core>,
    token: String,
    workers: Arc<Semaphore>,
}

/// Hold this independently of request tasks. The server never owns its Handle.
pub struct Handle {
    pub addr: SocketAddr,
    stop: watch::Sender<bool>,
    core: Arc<Core>,
    task: Mutex<Option<JoinHandle<Result<(), std::io::Error>>>>,
}

impl Handle {
    pub fn is_finished(&self) -> bool {
        self.task
            .lock()
            .map(|task| task.as_ref().is_none_or(JoinHandle::is_finished))
            .unwrap_or(true)
    }

    pub async fn stop(&self) -> Result<(), String> {
        self.shutdown(Duration::from_secs(5)).await
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<(), String> {
        self.core.stopped.store(true, Ordering::Release);
        self.stop.send_replace(true);
        let deadline = tokio::time::Instant::now() + grace;
        let core = self.core.clone();
        tokio::time::timeout_at(deadline, tokio::task::spawn_blocking(move || core.close()))
            .await
            .map_err(|_| "Continuity state shutdown timed out".to_owned())?
            .map_err(|_| "Continuity state shutdown failed".to_owned())?;
        let task = self
            .task
            .lock()
            .map_err(|_| "Continuity task is unavailable")?
            .take();
        if let Some(mut task) = task {
            match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => return Err(error.to_string()),
                Ok(Err(error)) => return Err(error.to_string()),
                Err(_) => {
                    task.abort();
                    return Err("Continuity HTTP shutdown timed out".into());
                }
            }
        }
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.core.stopped.store(true, Ordering::Release);
        self.stop.send_replace(true);
        let core = self.core.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || core.close());
        }
        // Never wait for filesystem work from Drop. Graceful shutdown remains
        // available to the owner; process exit closes the listener regardless.
    }
}

/// Called on the host's preparation worker with a live Tokio runtime. Opening
/// SQLite initializes only the bounded native schema; no history scan or network.
pub fn start(config: Config) -> Result<Arc<Handle>, String> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| "Continuity bridge requires a Tokio runtime")?;
    let token = config.token.clone();
    let core = Arc::new(Core::open(config)?);
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| error.to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let addr = listener.local_addr().map_err(|error| error.to_string())?;
    let _entered = runtime.enter();
    let listener = TcpListener::from_std(listener).map_err(|error| error.to_string())?;
    let state = Arc::new(StateData {
        core: core.clone(),
        token,
        workers: Arc::new(Semaphore::new(8)),
    });
    let app = Router::new()
        .route("/v1/continuity/prepare", post(prepare))
        .route("/v1/continuity/bind", post(bind))
        .route("/v1/continuity/observe", post(observe))
        .route("/v1/continuity/finish", post(finish))
        .route("/v1/continuity/release", post(release))
        // Go accepts raw events up to 50 MiB. The control envelope also carries
        // headers and JSON structure; it must not introduce a smaller wire cap.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(state);
    let (stop, mut stopping) = watch::channel(false);
    let task = runtime.spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !*stopping.borrow_and_update() {
                    if stopping.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });
    Ok(Arc::new(Handle {
        addr,
        stop,
        core,
        task: Mutex::new(Some(task)),
    }))
}

struct ApiError(ServiceError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let error = self.0;
        (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(serde_json::json!({
                "error": {"code": error.code, "message": error.message}
            })),
        )
            .into_response()
    }
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(supplied) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    supplied.len() == token.len()
        && supplied
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

async fn dispatch<T, R, F>(
    state: Arc<StateData>,
    headers: HeaderMap,
    payload: Result<Json<T>, JsonRejection>,
    operation: F,
) -> Result<Json<R>, ApiError>
where
    T: Send + 'static,
    R: Send + 'static,
    F: FnOnce(&Core, T) -> Result<R, ServiceError> + Send + 'static,
{
    if !authorized(&headers, &state.token) {
        return Err(ApiError(ServiceError::new(
            401,
            "invalid_continuity_token",
            "Continuity authorization is required",
        )));
    }
    let Json(payload) = payload.map_err(|rejection| {
        ApiError(ServiceError::new(
            rejection.status().as_u16(),
            "invalid_continuity_payload",
            "Invalid continuity JSON payload",
        ))
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let permit = tokio::time::timeout_at(deadline, state.workers.clone().acquire_owned())
        .await
        .map_err(|_| {
            ApiError(ServiceError::new(
                503,
                "continuity_worker_timeout",
                "Continuity state workers are busy",
            ))
        })?
        .map_err(|_| {
            ApiError(ServiceError::new(
                503,
                "continuity_bridge_stopping",
                "Continuity bridge is stopping",
            ))
        })?;
    let core = state.core.clone();
    // The permit belongs to the actual worker even if HTTP is cancelled or its
    // deadline expires. A retry uses the same idempotency/sequence identity.
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation(&core, payload)
    });
    tokio::time::timeout_at(deadline, worker)
        .await
        .map_err(|_| {
            ApiError(ServiceError::new(
                503,
                "continuity_operation_timeout",
                "Continuity state operation timed out",
            ))
        })?
        .map_err(|_| {
            ApiError(ServiceError::new(
                503,
                "continuity_state_unavailable",
                "Continuity state worker failed",
            ))
        })?
        .map(Json)
        .map_err(ApiError)
}

async fn prepare(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    payload: Result<Json<protocol::PrepareRequest>, JsonRejection>,
) -> Result<Json<protocol::PrepareResponse>, ApiError> {
    dispatch(state, headers, payload, Core::prepare).await
}
async fn bind(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    payload: Result<Json<protocol::BindRequest>, JsonRejection>,
) -> Result<Json<protocol::BindResponse>, ApiError> {
    dispatch(state, headers, payload, Core::bind).await
}
async fn observe(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    payload: Result<Json<protocol::ObserveRequest>, JsonRejection>,
) -> Result<Json<protocol::ObserveResponse>, ApiError> {
    dispatch(state, headers, payload, Core::observe).await
}
async fn finish(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    payload: Result<Json<protocol::FinishRequest>, JsonRejection>,
) -> Result<Json<protocol::FinishResponse>, ApiError> {
    dispatch(state, headers, payload, Core::finish).await
}

async fn release(
    State(state): State<Arc<StateData>>,
    headers: HeaderMap,
    payload: Result<Json<protocol::ReleaseRequest>, JsonRejection>,
) -> Result<Json<protocol::ReleaseResponse>, ApiError> {
    dispatch(state, headers, payload, Core::release).await
}

#[cfg(test)]
mod tests;
