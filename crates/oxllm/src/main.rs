use clap::{Parser, Subcommand};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use tower_http::cors::{Any, CorsLayer};

use oxllm_core::config::Config;
use oxllm_core::runtime::RuntimeMetrics;
use oxllm_core::state::{AppState, CircuitState, ProviderState};
use oxllm_core::telemetry::{TelemetryClient, TelemetryWorker};
use reqwest::Url;

/// Resolves the config file path using XDG base directory conventions.
///
/// If the given path exists, returns it as-is.
/// Otherwise, tries the XDG config path: `~/.config/oxllm/config.toml`
/// (respecting `$XDG_CONFIG_HOME` if set).
/// If that also doesn't exist, returns the original path so the caller
/// produces a clear file-not-found error.
fn resolve_config_path(given: PathBuf) -> PathBuf {
    if given.exists() {
        return given;
    }
    // Try XDG config path
    let xdg_config = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".config")
        })
        .join("oxllm")
        .join("config.toml");
    if xdg_config.exists() {
        return xdg_config;
    }
    given // fall back to original path (will produce a clear file-not-found error)
}

mod routes;

#[derive(Parser, Debug)]
#[command(
    name = "oxllm",
    version = env!("CARGO_PKG_VERSION"),
    author = "Nigel Jones",
    about = "Minimalist adaptive routing LLM proxy"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Starts the gateway Axum server and telemetry worker
    Serve {
        /// Path to the configuration TOML file
        /// (searches: <path>, ~/.config/oxllm/config.toml, ./config.toml)
        #[arg(short, long, default_value = "config.toml", env = "OXLLM_CONFIG")]
        config: PathBuf,

        /// Verbosity level. Repeat for more output: -v (debug), -vv (trace)
        #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 0)]
        verbose: u8,
    },
    /// Parses and validates the configuration syntax and cross-references
    Validate {
        /// Path to the configuration TOML file
        /// (searches: <path>, ~/.config/oxllm/config.toml, ./config.toml)
        #[arg(short, long, default_value = "config.toml", env = "OXLLM_CONFIG")]
        config: PathBuf,
    },
    /// Fetches and displays the active health and circuit status from localhost
    Status {
        /// Port of the running gateway server
        #[arg(short, long, default_value_t = 8080, env = "OXLLM_PORT")]
        port: u16,
    },
    /// Triggers hot-reload by sending SIGHUP to the running gateway daemon
    Reload {
        /// PID of the running oxllm process (optional, reads from /tmp/oxllm.pid by default)
        #[arg(short, long)]
        pid: Option<u32>,
    },
    /// Gracefully stops the running oxllm daemon (sends SIGTERM)
    Stop {
        /// PID of the running oxllm process (optional, reads from /tmp/oxllm.pid by default)
        #[arg(short, long)]
        pid: Option<u32>,
    },
    /// Manage providers (list, offline, online, reset)
    #[command(subcommand)]
    Provider(ProviderCommand),
}

#[derive(Subcommand, Debug)]
enum ProviderCommand {
    /// List all providers and their circuit state
    List {
        /// Port of the running gateway server
        #[arg(short, long, default_value_t = 8080, env = "OXLLM_PORT")]
        port: u16,
    },
    /// Take a provider offline (circuit breaker + manual disabled)
    Offline {
        /// Name of the provider to take offline
        name: String,
        /// Port of the running gateway server
        #[arg(short, long, default_value_t = 8080, env = "OXLLM_PORT")]
        port: u16,
    },
    /// Bring a provider back online
    Online {
        /// Name of the provider to bring online
        name: String,
        /// Port of the running gateway server
        #[arg(short, long, default_value_t = 8080, env = "OXLLM_PORT")]
        port: u16,
    },
    /// Reset a provider's circuit breaker, failures, and rate limit
    Reset {
        /// Name of the provider to reset
        name: String,
        /// Port of the running gateway server
        #[arg(short, long, default_value_t = 8080, env = "OXLLM_PORT")]
        port: u16,
    },
}

#[derive(Clone)]
pub struct Reloader {
    sender: tokio::sync::watch::Sender<Arc<AppState>>,
    config_path: PathBuf,
    metrics: Arc<RuntimeMetrics>,
}

#[derive(Clone)]
pub struct ReloadableState {
    pub app_state: tokio::sync::watch::Receiver<Arc<AppState>>,
    pub telemetry: TelemetryClient,
    pub start_time: Instant,
    pub reloader: Reloader,
}

impl axum::extract::FromRef<ReloadableState> for Arc<AppState> {
    fn from_ref(state: &ReloadableState) -> Self {
        state.app_state.borrow().clone()
    }
}

impl axum::extract::FromRef<ReloadableState> for (Arc<AppState>, TelemetryClient) {
    fn from_ref(state: &ReloadableState) -> Self {
        (state.app_state.borrow().clone(), state.telemetry.clone())
    }
}

impl axum::extract::FromRef<ReloadableState> for Instant {
    fn from_ref(state: &ReloadableState) -> Self {
        state.start_time
    }
}

impl axum::extract::FromRef<ReloadableState> for (Arc<AppState>, Instant) {
    fn from_ref(state: &ReloadableState) -> Self {
        (state.app_state.borrow().clone(), state.start_time)
    }
}

impl axum::extract::FromRef<ReloadableState> for Reloader {
    fn from_ref(state: &ReloadableState) -> Self {
        state.reloader.clone()
    }
}

fn build_app_state(config: Config, metrics: Arc<RuntimeMetrics>) -> Result<AppState, String> {
    let mut providers = Vec::new();
    for p in config.providers {
        if !p.enabled {
            continue;
        }
        let url = Url::parse(&p.base_url).map_err(|e| {
            format!(
                "Invalid base URL '{}' for provider '{}': {}",
                p.base_url, p.name, e
            )
        })?;

        providers.push(ProviderState {
            name: p.name,
            base_url: url,
            api_key: p.api_key,
            models: p.models,
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        });
    }

    // Virtual models take precedence over provider model names; warn so shadowing is explicit.
    for (virtual_name, targets) in &config.virtual_models {
        if providers
            .iter()
            .any(|provider| provider.models.iter().any(|model| model == virtual_name))
        {
            warn!(
                virtual_model = %virtual_name,
                "Virtual model shadows a real provider model name; virtual model routing takes precedence"
            );
        }
        if targets.is_empty() {
            // Validation rejects this; retain a defensive startup diagnostic if state is built directly.
            warn!(virtual_model = %virtual_name, "Virtual model has no routing targets");
        }
    }

    let http_client = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    Ok(AppState {
        providers,
        virtual_models: config.virtual_models,
        swrr_current: Mutex::new(std::collections::HashMap::new()),
        metrics,
        http_client,
        upstream_timeout_secs: config.server.upstream_timeout_secs,
    })
}

fn write_pid_file() -> std::io::Result<()> {
    let pid = std::process::id();
    std::fs::write("/tmp/oxllm.pid", pid.to_string())
}

fn send_sighup(pid: u32) -> std::io::Result<()> {
    let status = std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "kill command failed with exit code: {:?}",
            status.code()
        )))
    }
}

/// Generates a random request ID for response correlation.
fn generate_request_id() -> String {
    format!("oxllm-{:016x}", rand::random::<u64>())
}

/// Middleware that adds an `x-request-id` header to every response.
/// Generates the ID before calling the handler, stores it in request
/// extensions so route handlers can read the same ID for logs/telemetry,
/// and inserts it into the response header (without overriding an existing
/// `x-request-id` forwarded from upstream).
async fn add_request_id(mut req: Request<Body>, next: Next) -> Response {
    let request_id = generate_request_id();
    req.extensions_mut().insert(request_id.clone());
    let mut response = next.run(req).await;
    if !response.headers().contains_key("x-request-id") {
        // SAFETY: generate_request_id produces only ASCII hex chars and "oxllm-" prefix.
        response.headers_mut().insert(
            "x-request-id",
            HeaderValue::from_str(&request_id)
                .expect("generated request ID contains invalid characters"),
        );
    }
    response
}

fn is_tailnet_or_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => {
            let octets = ipv4.octets();
            ipv4.is_loopback() || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        },
        IpAddr::V6(ipv6) => match ipv6.to_ipv4_mapped() {
            Some(ipv4) => is_tailnet_or_loopback(IpAddr::V4(ipv4)),
            None => ipv6.is_loopback(),
        },
    }
}

async fn tailnet_only(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if is_tailnet_or_loopback(addr.ip()) {
        return next.run(req).await;
    }

    warn!(target: "oxllm::security", peer = %addr.ip(), "Blocked request from outside loopback and tailnet source ranges");
    let body = serde_json::json!({
        "error": {
            "message": "Access denied: source is outside the permitted tailnet and loopback ranges",
            "type": "forbidden",
            "code": 403
        }
    });
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::FORBIDDEN;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn health_check() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received SIGINT, shutting down gracefully...");
        },
        _ = terminate => {
            info!("Received SIGTERM, shutting down gracefully...");
        },
    }

    // Clean up PID file on shutdown
    let _ = std::fs::remove_file("/tmp/oxllm.pid");
}

async fn handle_http_reload(
    axum::extract::State(reloader): axum::extract::State<Reloader>,
) -> impl IntoResponse {
    let config = match Config::load_from_file(&reloader.config_path) {
        Ok(c) => c,
        Err(e) => {
            let body = serde_json::json!({
                "error": {
                    "message": format!("Failed to load config: {}", e),
                    "type": "invalid_request_error",
                    "code": 400
                }
            });
            let bytes = serde_json::to_vec(&body).unwrap();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            return response;
        },
    };
    if let Err(e) = config.validate() {
        let body = serde_json::json!({
            "error": {
                "message": format!("Config validation failed: {}", e),
                "type": "invalid_request_error",
                "code": 400
            }
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let mut response = Response::new(Body::from(bytes));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        return response;
    }
    match build_app_state(config, reloader.metrics.clone()) {
        Ok(new_state) => {
            if reloader.sender.send(Arc::new(new_state)).is_ok() {
                info!("Configuration reloaded via HTTP POST /reload");
                let body = serde_json::json!({
                    "message": "Configuration reloaded successfully"
                });
                let bytes = serde_json::to_vec(&body).unwrap();
                let mut response = Response::new(Body::from(bytes));
                *response.status_mut() = StatusCode::OK;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response
            } else {
                let body = serde_json::json!({
                    "error": {
                        "message": "Failed to send config update",
                        "type": "internal_error",
                        "code": 500
                    }
                });
                let bytes = serde_json::to_vec(&body).unwrap();
                let mut response = Response::new(Body::from(bytes));
                *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                response
            }
        },
        Err(e) => {
            let body = serde_json::json!({
                "error": {
                    "message": format!("Failed to build state: {}", e),
                    "type": "invalid_request_error",
                    "code": 400
                }
            });
            let bytes = serde_json::to_vec(&body).unwrap();
            let mut response = Response::new(Body::from(bytes));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        },
    }
}

async fn handle_sighup(
    config_path: PathBuf,
    watch_sender: tokio::sync::watch::Sender<Arc<AppState>>,
    metrics: Arc<RuntimeMetrics>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = match signal(SignalKind::hangup()) {
            Ok(s) => s,
            Err(e) => {
                error!("Failed to register SIGHUP handler: {}", e);
                return;
            },
        };

        info!("Registered SIGHUP reload listener");
        while sig.recv().await.is_some() {
            info!("SIGHUP received, reloading configuration...");
            match Config::load_from_file(&config_path) {
                Ok(new_config) => {
                    if let Err(e) = new_config.validate() {
                        error!("Configuration validation failed during hot-reload: {}", e);
                        continue;
                    }
                    match build_app_state(new_config, metrics.clone()) {
                        Ok(new_state) => {
                            if let Err(e) = watch_sender.send(Arc::new(new_state)) {
                                error!("Failed to update watch channel: {}", e);
                            } else {
                                info!("Configuration successfully reloaded!");
                            }
                        },
                        Err(e) => {
                            error!("Failed to build new app state during hot-reload: {}", e);
                        },
                    }
                },
                Err(e) => {
                    error!("Failed to load config file during hot-reload: {}", e);
                },
            }
        }
    }
}

fn build_router(reloadable_state: ReloadableState) -> axum::Router {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/v1/models", get(routes::list_models))
        .route("/v1/embeddings", post(routes::create_embeddings))
        .route(
            "/v1/chat/completions",
            post(routes::create_chat_completions),
        )
        .route("/status", get(routes::get_status))
        .route("/dashboard", get(routes::dashboard))
        .route("/health", get(health_check))
        .route("/reload", post(handle_http_reload))
        .route(
            "/admin/providers/{name}/offline",
            post(routes::admin_offline),
        )
        .route("/admin/providers/{name}/online", post(routes::admin_online))
        .route("/admin/providers/{name}/reset", post(routes::admin_reset))
        .layer(middleware::from_fn(tailnet_only))
        .layer(middleware::from_fn(add_request_id))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::OPTIONS,
                ])
                .allow_headers(Any),
        )
        .with_state(reloadable_state)
}

async fn run_serve(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = resolve_config_path(config_path);
    // 1. Load initial config
    let config = Config::load_from_file(&config_path)?;
    config.validate()?;

    // 2. Build initial AppState
    let metrics = Arc::new(RuntimeMetrics::default());
    let app_state = Arc::new(build_app_state(config.clone(), metrics.clone())?);

    // 3. Initialize watch channel
    let (watch_sender, watch_receiver) = tokio::sync::watch::channel(app_state.clone());

    // 4. Initialize telemetry channel & spawn TelemetryWorker
    let (telemetry_tx, telemetry_rx) = tokio::sync::mpsc::channel(1024);
    let telemetry_client = TelemetryClient::new(telemetry_tx);
    let otel_endpoint = config.server.otel_endpoint.clone();
    let _worker_handle = TelemetryWorker::spawn(&otel_endpoint, telemetry_rx)?;

    // 5. Spawn SIGHUP listener
    let config_path_clone = config_path.clone();
    tokio::spawn(handle_sighup(
        config_path_clone,
        watch_sender.clone(),
        metrics.clone(),
    ));

    // 6. Build Axum Router
    let start_time = Instant::now();
    let reloader = Reloader {
        sender: watch_sender.clone(),
        config_path: config_path.clone(),
        metrics: metrics.clone(),
    };
    let reloadable_state = ReloadableState {
        app_state: watch_receiver,
        telemetry: telemetry_client,
        start_time,
        reloader,
    };

    let app = build_router(reloadable_state);

    // Write PID file
    if let Err(e) = write_pid_file() {
        warn!("Failed to write PID file: {}", e);
    }

    // Bind all requested interfaces before serving so failures are fail-fast.
    let host: std::net::Ipv4Addr = config.server.host.parse()?;
    let port = config.server.port;
    let host_addr = SocketAddr::new(IpAddr::V4(host), port);
    let host_listener = tokio::net::TcpListener::bind(host_addr).await?;
    info!("Listening on http://{} (configured IPv4 host)", host_addr);

    let loopback_listener = if host.is_loopback() {
        None
    } else {
        let loopback_addr = SocketAddr::from(([127, 0, 0, 1], port));
        let listener = tokio::net::TcpListener::bind(loopback_addr).await?;
        info!("Listening on http://{} (loopback)", loopback_addr);
        Some(listener)
    };

    let make_service = app
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let host_server = axum::serve(host_listener, make_service);
    if let Some(loopback_listener) = loopback_listener {
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let mut host_shutdown = shutdown_receiver.clone();
        let mut loopback_shutdown = shutdown_receiver;
        let loopback_service = app.into_make_service_with_connect_info::<SocketAddr>();
        let mut host_handle = tokio::spawn(async move {
            host_server
                .with_graceful_shutdown(async move {
                    let _ = host_shutdown.changed().await;
                })
                .await
        });
        let mut loopback_handle = tokio::spawn(async move {
            axum::serve(loopback_listener, loopback_service)
                .with_graceful_shutdown(async move {
                    let _ = loopback_shutdown.changed().await;
                })
                .await
        });
        tokio::select! {
            result = &mut host_handle => {
                let _ = shutdown_sender.send(true);
                let loopback_result = loopback_handle.await?;
                result??;
                loopback_result?;
            },
            result = &mut loopback_handle => {
                let _ = shutdown_sender.send(true);
                let host_result = host_handle.await?;
                result??;
                host_result?;
            },
            _ = shutdown_signal() => {
                let _ = shutdown_sender.send(true);
                host_handle.await??;
                loopback_handle.await??;
            },
        }
    } else {
        host_server
            .with_graceful_shutdown(shutdown_signal())
            .await?;
    }

    Ok(())
}

fn run_validate(config_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = resolve_config_path(config_path);
    let config = Config::load_from_file(&config_path)?;
    config.validate()?;
    println!("Configuration file at '{:?}' is VALID!", config_path);
    Ok(())
}

async fn run_status(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/status", port);
    let res = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            println!("oxllm is not running on http://127.0.0.1:{}", port);
            println!("Start it with: oxllm serve");
            return Ok(());
        },
        Err(e) => return Err(e.into()),
    };
    if !res.status().is_success() {
        return Err(format!("Server returned error status: {}", res.status()).into());
    }

    #[derive(serde::Deserialize)]
    struct ProviderStatus {
        name: String,
        models: String,
        circuit: String,
        failures: u32,
        rate_limited: bool,
        requests: u64,
        successes: u64,
        tokens_input: u64,
        tokens_output: u64,
        last_request: String,
    }

    #[derive(serde::Deserialize)]
    struct RouteEntry {
        provider: String,
        model: String,
        circuit: String,
        requests: u64,
        successes: u64,
    }

    #[derive(serde::Deserialize)]
    struct StatusResponse {
        uptime_secs: u64,
        total_requests: u64,
        providers: Vec<ProviderStatus>,
        virtual_models: std::collections::HashMap<String, Vec<RouteEntry>>,
    }

    let status: StatusResponse = res.json().await?;

    let uptime_mins = status.uptime_secs / 60;
    let uptime_secs_rem = status.uptime_secs % 60;
    println!(
        "\nUptime: {}m {}s  |  Total Requests: {}",
        uptime_mins, uptime_secs_rem, status.total_requests
    );

    // Virtual model routing tables
    for (vm_name, routes) in &status.virtual_models {
        println!("\nVirtual Model: {}", vm_name);
        println!("{}", "-".repeat(127));
        println!(
            "| {:<20} | {:<45} | {:<30} | {:>8} | {:>8} |",
            "Provider", "Model", "Circuit", "Requests", "Success"
        );
        println!("{}", "-".repeat(127));
        for entry in routes {
            println!(
                "| {:<20} | {:<45} | {:<30} | {:>8} | {:>8} |",
                entry.provider, entry.model, entry.circuit, entry.requests, entry.successes
            );
        }
    }

    // Per-provider table
    println!(
        "\n+--------------------+-----------------------------------------------+--------------------------------+----------+---------------+----------+-----------+--------------+---------------+--------------+"
    );
    println!("| Provider Name      | Models                                                | Circuit Breaker State          | Failures | Rate Limited? | Requests | Successes | Tokens Input | Tokens Output | Last Request|");
    println!("+--------------------+-----------------------------------------------+--------------------------------+----------+---------------+----------+-----------+--------------+---------------+--------------+");
    for s in &status.providers {
        println!(
            "| {:<18} | {:<45} | {:<30} | {:<8} | {:<13} | {:<8} | {:<9} | {:<12} | {:<13} | {:<12} |",
            s.name,
            s.models,
            s.circuit,
            s.failures,
            if s.rate_limited { "Yes" } else { "No" },
            s.requests,
            s.successes,
            s.tokens_input,
            s.tokens_output,
            s.last_request,
        );
    }
    println!(
        "+--------------------+-----------------------------------------------+--------------------------------+----------+---------------+----------+-----------+--------------+---------------+--------------+\n"
    );

    Ok(())
}
fn run_reload(pid_opt: Option<u32>) -> Result<(), Box<dyn std::error::Error>> {
    let pid = match pid_opt {
        Some(p) => p,
        None => match std::fs::read_to_string("/tmp/oxllm.pid") {
            Ok(content) => match content.trim().parse::<u32>() {
                Ok(p) => p,
                Err(_) => {
                    println!("Invalid PID in /tmp/oxllm.pid");
                    return Ok(());
                },
            },
            Err(_) => {
                println!("oxllm is not running (no PID file at /tmp/oxllm.pid)");
                println!("Start it with: oxllm serve");
                return Ok(());
            },
        },
    };
    send_sighup(pid)?;
    println!(
        "Successfully sent hot-reload signal (SIGHUP) to process {}",
        pid
    );
    Ok(())
}

fn run_stop(pid_opt: Option<u32>) -> Result<(), Box<dyn std::error::Error>> {
    let pid = match pid_opt {
        Some(p) => p,
        None => match std::fs::read_to_string("/tmp/oxllm.pid") {
            Ok(content) => match content.trim().parse::<u32>() {
                Ok(p) => p,
                Err(_) => {
                    println!("Invalid PID in /tmp/oxllm.pid");
                    return Ok(());
                },
            },
            Err(_) => {
                println!("oxllm is not running (no PID file at /tmp/oxllm.pid)");
                println!("Start it with: oxllm serve");
                return Ok(());
            },
        },
    };

    let status = match std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
    {
        Ok(s) => s,
        Err(e) => {
            println!("Failed to send SIGTERM: {}", e);
            return Ok(());
        },
    };
    if status.success() {
        println!(
            "Successfully sent graceful shutdown signal (SIGTERM) to process {}",
            pid
        );
    } else {
        println!("kill command failed with exit code: {:?}", status.code());
    }
    Ok(())
}

async fn run_provider_offline(name: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/admin/providers/{}/offline", port, name);
    let res = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            println!("oxllm is not running on http://127.0.0.1:{}", port);
            println!("Start it with: oxllm serve");
            return Ok(());
        },
        Err(e) => return Err(e.into()),
    };
    let body = res.text().await?;
    println!("{}", body);
    Ok(())
}

async fn run_provider_online(name: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/admin/providers/{}/online", port, name);
    let res = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            println!("oxllm is not running on http://127.0.0.1:{}", port);
            println!("Start it with: oxllm serve");
            return Ok(());
        },
        Err(e) => return Err(e.into()),
    };
    let body = res.text().await?;
    println!("{}", body);
    Ok(())
}

async fn run_provider_reset(name: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/admin/providers/{}/reset", port, name);
    let res = match client.post(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            println!("oxllm is not running on http://127.0.0.1:{}", port);
            println!("Start it with: oxllm serve");
            return Ok(());
        },
        Err(e) => return Err(e.into()),
    };
    let body = res.text().await?;
    println!("{}", body);
    Ok(())
}

async fn run_provider_list(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{}/status", port);
    let res = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) if e.is_connect() => {
            println!("oxllm is not running on http://127.0.0.1:{}", port);
            println!("Start it with: oxllm serve");
            return Ok(());
        },
        Err(e) => return Err(e.into()),
    };

    #[derive(serde::Deserialize)]
    struct RouteEntry {
        provider: String,
        model: String,
        circuit: String,
        requests: u64,
        successes: u64,
    }

    #[derive(serde::Deserialize)]
    struct StatusResponse {
        virtual_models: std::collections::HashMap<String, Vec<RouteEntry>>,
    }

    let status: StatusResponse = res.json().await?;

    // Deduplicate providers across virtual models, keeping the first occurrence
    // to show each provider once with its model and circuit state
    let mut seen = std::collections::HashSet::new();
    let mut providers: Vec<&RouteEntry> = Vec::new();

    for routes in status.virtual_models.values() {
        for entry in routes {
            if seen.insert(&entry.provider) {
                providers.push(entry);
            }
        }
    }

    println!();
    println!("+----------------------+-----------------------------------------------+--------------------------------+----------+----------+");
    println!("| Provider             | Model                                         | Circuit                        | Requests | Success  |");
    println!("+----------------------+-----------------------------------------------+--------------------------------+----------+----------+");
    for entry in &providers {
        let icon = if entry.circuit.starts_with("Closed") {
            '✓'
        } else if entry.circuit.starts_with("Half") {
            '⧖'
        } else {
            '✗'
        };
        println!(
            "| {:<20} | {:<45} | {:<30} | {:>8} | {:>8} |",
            format!("{} {}", icon, entry.provider),
            entry.model,
            entry.circuit,
            entry.requests,
            entry.successes,
        );
    }
    println!("+----------------------+-----------------------------------------------+--------------------------------+----------+----------+");
    println!();
    println!("Use 'oxllm provider offline <name>' to take a provider out of rotation.");
    println!("Use 'oxllm provider reset <name>' to clear circuit breaker state.");
    println!();

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Build the log filter based on verbosity level
    let log_filter = match &cli.command {
        Commands::Serve { verbose, .. } => match verbose {
            0 => "info,oxllm=info,oxllm_core=info",
            1 => "info,oxllm=debug,oxllm_core=debug",
            _ => "trace",
        },
        _ => "info,oxllm=debug,oxllm_core=debug",
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_filter));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    match cli.command {
        Commands::Serve { config, .. } => {
            run_serve(config).await?;
        },
        Commands::Validate { config } => {
            run_validate(config)?;
        },
        Commands::Status { port } => {
            run_status(port).await?;
        },
        Commands::Reload { pid } => {
            run_reload(pid)?;
        },
        Commands::Stop { pid } => {
            run_stop(pid)?;
        },
        Commands::Provider(cmd) => match cmd {
            ProviderCommand::List { port } => {
                run_provider_list(port).await?;
            },
            ProviderCommand::Offline { name, port } => {
                run_provider_offline(&name, port).await?;
            },
            ProviderCommand::Online { name, port } => {
                run_provider_online(&name, port).await?;
            },
            ProviderCommand::Reset { name, port } => {
                run_provider_reset(&name, port).await?;
            },
        },
    }

    Ok(())
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use oxllm_core::config::VirtualModelTarget;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn tailnet_only_allows_only_loopback_and_ipv4_cgnat() {
        let allowed = [
            "127.0.0.1",
            "::1",
            "::ffff:127.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "::ffff:100.64.0.1",
        ];
        for address in allowed {
            let ip = address.parse().expect("test IP literal must parse");
            assert!(
                is_tailnet_or_loopback(ip),
                "expected {address} to be allowed"
            );
        }

        let rejected = [
            "8.8.8.8",
            "192.168.1.10",
            "100.0.0.1",
            "100.128.0.0",
            "2001:db8::1",
            "fd7a:115c:a1e0::1",
        ];
        for address in rejected {
            let ip = address.parse().expect("test IP literal must parse");
            assert!(
                !is_tailnet_or_loopback(ip),
                "expected {address} to be rejected"
            );
        }
    }

    async fn spawn_mock_upstream(responses: Vec<String>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let responses = Arc::new(responses);
            let mut idx = 0;

            while let Ok((mut stream, _)) = listener.accept().await {
                let responses = responses.clone();
                let current_idx = idx;
                idx += 1;

                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;

                    let response = if responses.is_empty() {
                        "HTTP/1.1 500 Internal Error\r\nContent-Length: 0\r\n\r\n"
                    } else {
                        let r_idx = current_idx.min(responses.len() - 1);
                        &responses[r_idx]
                    };

                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.flush().await;
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn test_integration_rate_limit_failover() {
        let upstream1_resp = vec![
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\n\r\n"
                .to_string(),
        ];
        let upstream2_resp = vec![
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 75\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"Hello from prov2\"}}]}".to_string()
        ];

        let addr1 = spawn_mock_upstream(upstream1_resp).await;
        let addr2 = spawn_mock_upstream(upstream2_resp).await;

        let p1 = ProviderState {
            name: "prov1".to_string(),
            base_url: Url::parse(&format!("http://{}", addr1)).unwrap(),
            api_key: "key1".to_string(),
            models: vec!["gpt-4-upstream".to_string()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let p2 = ProviderState {
            name: "prov2".to_string(),
            base_url: Url::parse(&format!("http://{}", addr2)).unwrap(),
            api_key: "key2".to_string(),
            models: vec!["gpt-4-upstream".to_string()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "gpt-4".to_string(),
            vec![
                VirtualModelTarget {
                    provider: "prov1".to_string(),
                    model: "gpt-4-upstream".to_string(),
                    weight: 1,
                },
                VirtualModelTarget {
                    provider: "prov2".to_string(),
                    model: "gpt-4-upstream".to_string(),
                    weight: 1,
                },
            ],
        );

        let http_client = reqwest::Client::builder().build().unwrap();
        let app_state = Arc::new(AppState {
            providers: vec![p1, p2],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client,
            upstream_timeout_secs: 5,
        });

        let (_watch_sender, watch_receiver) = tokio::sync::watch::channel(app_state.clone());
        let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(1024);
        let telemetry_client = TelemetryClient::new(telemetry_tx);

        let dummy_reloader = Reloader {
            sender: _watch_sender.clone(),
            config_path: PathBuf::from("config.toml"),
            metrics: Arc::new(RuntimeMetrics::default()),
        };

        let reloadable_state = ReloadableState {
            app_state: watch_receiver,
            telemetry: telemetry_client,
            start_time: Instant::now(),
            reloader: dummy_reloader,
        };

        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(reloadable_state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}]
        });

        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&payload)
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 200);

        let body: Value = res.json().await.unwrap();
        let content = body["choices"][0]["message"]["content"].as_str().unwrap();
        assert_eq!(content, "Hello from prov2");

        let p1_limited = app_state.providers[0].rate_limited_until.read().await;
        assert!(p1_limited.is_some());
    }

    #[tokio::test]
    async fn test_integration_sse_streaming() {
        let sse_response = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {\"token\": \"Hello\"}\n\ndata: {\"token\": \" World\"}\n\n".to_string();

        let addr = spawn_mock_upstream(vec![sse_response]).await;

        let p = ProviderState {
            name: "prov1".to_string(),
            base_url: Url::parse(&format!("http://{}", addr)).unwrap(),
            api_key: "key1".to_string(),
            models: vec!["gpt-4-upstream".to_string()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "gpt-4".to_string(),
            vec![VirtualModelTarget {
                provider: "prov1".to_string(),
                model: "gpt-4-upstream".to_string(),
                weight: 1,
            }],
        );

        let http_client = reqwest::Client::builder().build().unwrap();
        let app_state = Arc::new(AppState {
            providers: vec![p],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client,
            upstream_timeout_secs: 5,
        });

        let (_watch_sender, watch_receiver) = tokio::sync::watch::channel(app_state.clone());
        let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(1024);
        let telemetry_client = TelemetryClient::new(telemetry_tx);

        let dummy_reloader = Reloader {
            sender: _watch_sender.clone(),
            config_path: PathBuf::from("config.toml"),
            metrics: Arc::new(RuntimeMetrics::default()),
        };

        let reloadable_state = ReloadableState {
            app_state: watch_receiver,
            telemetry: telemetry_client,
            start_time: Instant::now(),
            reloader: dummy_reloader,
        };

        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(reloadable_state);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let payload = serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });

        let mut res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&payload)
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "text/event-stream"
        );

        let mut body = String::new();
        while let Some(chunk) = res.chunk().await.unwrap() {
            body.push_str(std::str::from_utf8(&chunk).unwrap());
        }

        assert!(body.contains("data: {\"token\": \"Hello\"}"));
        assert!(body.contains("data: {\"token\": \" World\"}"));
    }

    // -----------------------------------------------------------------------
    // New integration tests: timeout failover, all-providers-fail, manual
    // offline, rate-limit recovery, embeddings endpoint
    // -----------------------------------------------------------------------

    /// Circuit breaker trips after 3 failures; failover routes to healthy provider.
    #[tokio::test]
    async fn test_integration_circuit_breaker_failover() {
        use oxllm_core::router::{AdaptivePriorityStrategy, RoutingStrategy};

        let p1 = ProviderState {
            name: "primary".into(),
            base_url: Url::parse("https://api.fail.example.com/v1/").unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let p2 = ProviderState {
            name: "secondary".into(),
            base_url: Url::parse("https://api.ok.example.com/v1/").unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let strategy = AdaptivePriorityStrategy;
        let candidates = vec![&p1, &p2];

        // Initially selects primary
        let selected = strategy.select(&candidates).await.unwrap();
        assert_eq!(selected.name, "primary");

        // Simulate 3 failures on primary
        for _ in 0..3 {
            strategy.feedback(&p1, false, false, Some(500), None).await;
        }

        // Circuit should be open now; select should skip to secondary
        assert!(matches!(
            *p1.circuit.read().await,
            CircuitState::Open { .. }
        ));
        let selected = strategy.select(&candidates).await.unwrap();
        assert_eq!(selected.name, "secondary");
    }

    /// When every provider fails, the proxy returns 502 Bad Gateway.
    #[tokio::test]
    async fn test_integration_all_providers_fail_gives_502() {
        let fail_response =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(fail_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let make_provider = |name: &str| ProviderState {
            name: name.into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let p1 = make_provider("p1");
        let p2 = make_provider("p2");

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "test".into(),
            vec![
                VirtualModelTarget {
                    provider: "p1".into(),
                    model: "model".into(),
                    weight: 1,
                },
                VirtualModelTarget {
                    provider: "p2".into(),
                    model: "model".into(),
                    weight: 1,
                },
            ],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p1, p2],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });

        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let state = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&serde_json::json!({
                "model": "test",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 502);
        let body = res.text().await.unwrap();
        assert!(body.contains("All upstream chat completions providers failed"));
    }

    /// Manual offline takes a provider out of rotation; requests bypass it.
    #[tokio::test]
    async fn test_integration_manual_offline_bypasses_provider() {
        use std::sync::atomic::Ordering;

        let ok_response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}"
                .to_string();

        // Two independent upstreams
        let p1_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p1_addr = p1_listener.local_addr().unwrap();
        let ok_response_p1 = ok_response.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = p1_listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                // Should never be hit — but respond just in case
                let _ = stream.write_all(ok_response_p1.as_bytes()).await;
            }
        });

        let p2_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p2_addr = p2_listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = p2_listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p1 = ProviderState {
            name: "p1".into(),
            base_url: Url::parse(&format!("http://{}/v1/", p1_addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(true), // MANUALLY DISABLED
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let p2 = ProviderState {
            name: "p2".into(),
            base_url: Url::parse(&format!("http://{}/v1/", p2_addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "test".into(),
            vec![
                VirtualModelTarget {
                    provider: "p1".into(),
                    model: "model".into(),
                    weight: 1,
                },
                VirtualModelTarget {
                    provider: "p2".into(),
                    model: "model".into(),
                    weight: 1,
                },
            ],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p1, p2],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });

        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let state = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&serde_json::json!({
                "model": "test",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // p1 was never hit; p2 handled the request
        assert_eq!(app_state.providers[0].requests.load(Ordering::Relaxed), 0);
        assert_eq!(app_state.providers[1].requests.load(Ordering::Relaxed), 1);
    }

    /// Admin reset endpoint clears circuit breaker, failures, and rate limit.
    #[tokio::test]
    async fn test_integration_admin_reset() {
        use std::sync::atomic::Ordering;

        let ok_response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}"
                .to_string();
        let ok_response_clone = ok_response.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response_clone.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p = ProviderState {
            name: "resetme".into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "test".into(),
            vec![VirtualModelTarget {
                provider: "resetme".into(),
                model: "model".into(),
                weight: 1,
            }],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });

        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let state = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };

        // Simulate a tripped state
        *app_state.providers[0].circuit.write().await = CircuitState::Open {
            until: Instant::now() + Duration::from_secs(300),
        };
        *app_state.providers[0].consecutive_failures.write().await = 5;
        *app_state.providers[0].rate_limited_until.write().await =
            Some(Instant::now() + Duration::from_secs(300));
        app_state.providers[0]
            .manual_disabled
            .store(true, Ordering::Release);

        // Build router with admin routes
        let router = axum::Router::new()
            .route(
                "/admin/providers/{name}/reset",
                axum::routing::post(routes::admin_reset),
            )
            .with_state(state.clone());
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Call admin reset
        let client = reqwest::Client::new();
        let res = client
            .post(format!(
                "http://{}/admin/providers/resetme/reset",
                proxy_addr
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Verify all state is cleared
        assert_eq!(
            *app_state.providers[0].circuit.read().await,
            CircuitState::Closed
        );
        assert_eq!(*app_state.providers[0].consecutive_failures.read().await, 0);
        assert!(app_state.providers[0]
            .rate_limited_until
            .read()
            .await
            .is_none());
        assert!(!app_state.providers[0]
            .manual_disabled
            .load(Ordering::Acquire));
    }

    /// /v1/embeddings endpoint proxies correctly and returns embeddings.
    #[tokio::test]
    async fn test_integration_embeddings_success() {
        let body = r#"{"data":[{"embedding":[0.1,0.2,0.3],"index":0}],"model":"test","usage":{"prompt_tokens":4}}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p = ProviderState {
            name: "emb".into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut virtual_models = std::collections::HashMap::new();
        virtual_models.insert(
            "test-emb".into(),
            vec![VirtualModelTarget {
                provider: "emb".into(),
                model: "model".into(),
                weight: 1,
            }],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p],
            virtual_models,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });

        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let state = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .route(
                "/v1/embeddings",
                axum::routing::post(routes::create_embeddings),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/embeddings", proxy_addr))
            .json(&serde_json::json!({
                "model": "test-emb",
                "input": "hello"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let body: Value = res.json().await.unwrap();
        assert_eq!(body["data"][0]["embedding"].as_array().unwrap().len(), 3);
    }

    /// Invalid model name returns 400 Bad Request.
    #[tokio::test]
    async fn test_integration_invalid_model_returns_400() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p = ProviderState {
            name: "prov".into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["real-model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };
        let mut vm = std::collections::HashMap::new();
        vm.insert(
            "known-model".into(),
            vec![VirtualModelTarget {
                provider: "prov".into(),
                model: "real-model".into(),
                weight: 1,
            }],
        );

        let state = Arc::new(AppState {
            providers: vec![p],
            virtual_models: vm,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });
        let (_ws, wr) = tokio::sync::watch::channel(state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let rs = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .layer(middleware::from_fn(add_request_id))
            .with_state(rs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client.post(format!("http://{}/v1/chat/completions", addr))
            .json(&serde_json::json!({"model": "nonexistent", "messages": [{"role":"user","content":"hi"}]}))
            .send().await.unwrap();
        assert_eq!(res.status(), 400);
    }

    /// Admin online re-enables a manually disabled provider.
    #[tokio::test]
    async fn test_integration_admin_online() {
        use std::sync::atomic::Ordering;

        let ok_response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}"
                .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p = ProviderState {
            name: "target".into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(true), // Start disabled
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut vms = std::collections::HashMap::new();
        vms.insert(
            "test".into(),
            vec![VirtualModelTarget {
                provider: "target".into(),
                model: "model".into(),
                weight: 1,
            }],
        );
        let state = Arc::new(AppState {
            providers: vec![p],
            virtual_models: vms,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });
        let (_ws, wr) = tokio::sync::watch::channel(state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let rs = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws.clone(),
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/admin/providers/{name}/online",
                axum::routing::post(routes::admin_online),
            )
            .with_state(rs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Verify provider is disabled
        assert!(state.providers[0].manual_disabled.load(Ordering::Acquire));

        // Call admin online
        let client = reqwest::Client::new();
        let res = client
            .post(format!(
                "http://{}/admin/providers/target/online",
                proxy_addr
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Verify provider is now enabled
        assert!(!state.providers[0].manual_disabled.load(Ordering::Acquire));
    }

    /// Token counts are parsed from upstream JSON and recorded in provider counters.
    #[tokio::test]
    async fn test_integration_token_count_parsed() {
        // Upstream returns a response with usage data
        let body = r#"{"id":"test","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":42,"prompt_tokens_details":{"cached_tokens":10},"completion_tokens":7,"total_tokens":49}}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let p = ProviderState {
            name: "counter".into(),
            base_url: Url::parse(&format!("http://{}/v1/", addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };

        let mut vms = std::collections::HashMap::new();
        vms.insert(
            "test".into(),
            vec![VirtualModelTarget {
                provider: "counter".into(),
                model: "model".into(),
                weight: 1,
            }],
        );
        let state = Arc::new(AppState {
            providers: vec![p],
            virtual_models: vms,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });
        let (_ws, wr) = tokio::sync::watch::channel(state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let rs = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        let router = axum::Router::new()
            .route(
                "/v1/chat/completions",
                axum::routing::post(routes::create_chat_completions),
            )
            .route("/status", axum::routing::get(routes::get_status))
            .route("/dashboard", axum::routing::get(routes::dashboard))
            .layer(middleware::from_fn(add_request_id))
            .with_state(rs);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(
                &serde_json::json!({"model": "test", "messages": [{"role":"user","content":"hi"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);

        // Verify token counts were parsed
        let tokens_in = state.providers[0]
            .tokens_input
            .load(std::sync::atomic::Ordering::Relaxed);
        let tokens_out = state.providers[0]
            .tokens_output
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            tokens_in > 0,
            "Expected input tokens > 0, got {}",
            tokens_in
        );
        assert!(
            tokens_out > 0,
            "Expected output tokens > 0, got {}",
            tokens_out
        );
        assert_eq!(tokens_in, 42);
        assert_eq!(tokens_out, 7);

        // Daily accounting parses cached details from the provider usage object.
        let tokens = state.metrics.daily_tokens.snapshot();
        assert_eq!(tokens.cached_tokens, 10);
        assert_eq!(tokens.uncached_tokens, 32);
        assert_eq!(tokens.total_tokens, 49);

        // The request log captured this request with exactly the handoff fields
        let log = state.metrics.recent_requests();
        assert_eq!(log.len(), 1);
        let entry = &log[0];
        assert_eq!(entry.model_requested, "test");
        assert_eq!(entry.virtual_model.as_deref(), Some("test"));
        assert_eq!(entry.provider, "counter");
        assert_eq!(entry.cached_tokens, 10);
        assert_eq!(entry.uncached_tokens, 32);
        assert_eq!(entry.status_code, 200);
        assert!(entry.timestamp > 0);

        // /status exposes the new fields (dashboard feed); /dashboard serves HTML
        let status: Value = client
            .get(format!("http://{}/status", proxy_addr))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["daily_tokens"]["total_tokens"], 49);
        assert_eq!(status["recent_requests"][0]["model_requested"], "test");
        assert_eq!(status["recent_requests"][0]["virtual_model"], "test");
        assert_eq!(status["virtual_models"]["test"][0]["weight"], 1);

        let page = client
            .get(format!("http://{}/dashboard", proxy_addr))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        assert!(page
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        let html = page.text().await.unwrap();
        assert!(html.contains("Cached tokens"));
        assert!(html.contains("Virtual model routing"));
        assert!(html.contains("data.virtual_models"));
        assert!(html.contains("target.weight"));
        assert!(html.contains("Recent requests"));
    }

    // -----------------------------------------------------------------------
    // Test helpers for JSON error format, CORS, and x-request-id tests
    // -----------------------------------------------------------------------

    /// Builds a minimal test AppState + ReloadableState with one provider
    /// pointing at the given upstream address.
    fn build_test_state(upstream_addr: SocketAddr) -> (Arc<AppState>, ReloadableState) {
        let p = ProviderState {
            name: "prov".into(),
            base_url: Url::parse(&format!("http://{}/v1/", upstream_addr)).unwrap(),
            api_key: "key".into(),
            models: vec!["model".into()],
            circuit: Arc::new(RwLock::new(CircuitState::Closed)),
            consecutive_failures: Arc::new(RwLock::new(0)),
            rate_limited_until: Arc::new(RwLock::new(None)),
            last_attempt_time: Arc::new(RwLock::new(None)),
            probe_in_flight: Arc::new(AtomicBool::new(false)),
            manual_disabled: AtomicBool::new(false),
            requests: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            tokens_input: AtomicU64::new(0),
            tokens_output: AtomicU64::new(0),
        };
        let mut vm = std::collections::HashMap::new();
        vm.insert(
            "test".into(),
            vec![VirtualModelTarget {
                provider: "prov".into(),
                model: "model".into(),
                weight: 1,
            }],
        );

        let app_state = Arc::new(AppState {
            providers: vec![p],
            virtual_models: vm,
            swrr_current: Mutex::new(std::collections::HashMap::new()),
            metrics: Arc::new(RuntimeMetrics::default()),
            http_client: reqwest::Client::builder().build().unwrap(),
            upstream_timeout_secs: 5,
        });
        let (_ws, wr) = tokio::sync::watch::channel(app_state.clone());
        let (ttx, _trx) = tokio::sync::mpsc::channel(1024);
        let rs = ReloadableState {
            app_state: wr,
            telemetry: TelemetryClient::new(ttx),
            start_time: Instant::now(),
            reloader: Reloader {
                sender: _ws,
                config_path: PathBuf::from("."),
                metrics: Arc::new(RuntimeMetrics::default()),
            },
        };
        (app_state, rs)
    }

    /// Configures a request as if it arrived from the given peer address.
    fn request_from_peer(uri: &str, peer: IpAddr) -> Request<Body> {
        let mut request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("static test request must build");
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(peer, 0)));
        request
    }

    #[tokio::test]
    async fn dual_listener_serves_configured_and_loopback_addresses() {
        let listeners = bind_server_listeners("127.0.0.2".parse().expect("static IP"), 0)
            .await
            .expect("loopback listeners should bind");
        assert_eq!(listeners.len(), 2);
        let host_addr = listeners[0].local_addr().expect("listener address");
        let loopback_addr = listeners[1].local_addr().expect("listener address");
        assert_eq!(host_addr.ip(), IpAddr::from([127, 0, 0, 2]));
        assert_eq!(loopback_addr.ip(), IpAddr::from([127, 0, 0, 1]));
        assert_eq!(host_addr.port(), loopback_addr.port());

        let upstream = spawn_mock_upstream(vec![]).await;
        let (_state, reloadable_state) = build_test_state(upstream);
        let router = build_router(reloadable_state);
        let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
        let mut shutdown_host = shutdown_receiver.clone();
        let mut shutdown_loopback = shutdown_receiver;
        let host_router = router.clone();
        let loopback_router = router;
        let host_task = tokio::spawn(async move {
            axum::serve(
                listeners[0].try_clone().expect("clone host listener"),
                host_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_host.changed().await;
            })
            .await
        });
        let loopback_task = tokio::spawn(async move {
            axum::serve(
                listeners[1].try_clone().expect("clone loopback listener"),
                loopback_router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                let _ = shutdown_loopback.changed().await;
            })
            .await
        });

        let client = reqwest::Client::new();
        for address in [host_addr, loopback_addr] {
            let response = client
                .get(format!("http://{address}/health"))
                .send()
                .await
                .expect("health request should connect");
            assert_eq!(response.status(), StatusCode::OK);
        }
        let _ = shutdown_sender.send(true);
        host_task
            .await
            .expect("host server task")
            .expect("host server");
        loopback_task
            .await
            .expect("loopback server task")
            .expect("loopback server");
    }

    #[tokio::test]
    async fn router_builds_and_guards_all_routes() {
        use tower::util::ServiceExt;

        let upstream = spawn_mock_upstream(vec![]).await;
        let (_state, reloadable_state) = build_test_state(upstream);
        let router = build_router(reloadable_state);

        // Loopback peer reaches every production route without auth.
        for uri in [
            "/v1/models",
            "/status",
            "/dashboard",
            "/health",
            "/v1/embeddings",
        ] {
            let response = router
                .clone()
                .oneshot(request_from_peer(uri, IpAddr::from([127, 0, 0, 1])))
                .await
                .expect("router must answer a loopback request");
            assert!(
                response.status() != StatusCode::FORBIDDEN,
                "loopback peer unexpectedly denied on {uri}"
            );
        }

        // A public source is denied on every route, including /v1.
        for uri in [
            "/v1/models",
            "/status",
            "/dashboard",
            "/health",
            "/reload",
            "/admin/providers/prov/offline",
            "/v1/chat/completions",
        ] {
            let response = router
                .clone()
                .oneshot(request_from_peer(uri, IpAddr::from([8, 8, 8, 8])))
                .await
                .expect("router must answer a forged peer request");
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "public source unexpectedly reached {uri}"
            );
            assert!(
                response.headers().contains_key("x-request-id"),
                "403 on {uri} lost the x-request-id header"
            );
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&HeaderValue::from_static("application/json")),
                "403 on {uri} is not JSON"
            );
        }
    }

    /// Uses the exact router and security middleware wired by production.
    fn build_test_router(state: ReloadableState) -> axum::Router {
        build_router(state)
    }

    // -----------------------------------------------------------------------
    // Integration tests: JSON error format
    // -----------------------------------------------------------------------

    /// Invalid JSON body returns 400 with structured JSON error.
    #[tokio::test]
    async fn test_integration_invalid_json() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .body("not valid json {{{")
            .header("Content-Type", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 400);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body: Value = res.json().await.unwrap();
        let error = body.get("error").expect("response should have 'error' key");
        assert!(error.get("message").is_some());
        assert_eq!(
            error.get("type").unwrap().as_str().unwrap(),
            "invalid_request_error"
        );
        assert_eq!(error.get("code").unwrap().as_u64().unwrap(), 400);
    }

    /// Missing 'model' field returns 400 with structured JSON error.
    #[tokio::test]
    async fn test_integration_missing_model() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&serde_json::json!({
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 400);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body: Value = res.json().await.unwrap();
        let error = body.get("error").expect("response should have 'error' key");
        assert!(error.get("message").is_some());
        assert_eq!(
            error.get("type").unwrap().as_str().unwrap(),
            "invalid_request_error"
        );
        assert_eq!(error.get("code").unwrap().as_u64().unwrap(), 400);
    }

    /// Unknown virtual model returns 400 with structured JSON error.
    #[tokio::test]
    async fn test_integration_unknown_model() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 400);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/json"
        );

        let body: Value = res.json().await.unwrap();
        let error = body.get("error").expect("response should have 'error' key");
        let message = error.get("message").unwrap().as_str().unwrap();
        assert!(message.contains("nonexistent-model"));
        assert_eq!(
            error.get("type").unwrap().as_str().unwrap(),
            "invalid_request_error"
        );
        assert_eq!(error.get("code").unwrap().as_u64().unwrap(), 400);
    }

    // -----------------------------------------------------------------------
    // Integration tests: CORS headers
    // -----------------------------------------------------------------------

    /// OPTIONS preflight to /v1/chat/completions returns CORS headers.
    #[tokio::test]
    async fn test_integration_cors_options() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .request(
                reqwest::Method::OPTIONS,
                format!("http://{}/v1/chat/completions", proxy_addr),
            )
            .header("Origin", "http://localhost:3000")
            .header("Access-Control-Request-Method", "POST")
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
        assert!(res.headers().contains_key("access-control-allow-methods"));
    }

    /// POST to /v1/chat/completions includes CORS origin header in response.
    #[tokio::test]
    async fn test_integration_cors_post() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Hello from prov"}}]}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .header("Origin", "http://localhost:3000")
            .json(&serde_json::json!({
                "model": "test",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get("access-control-allow-origin").unwrap(),
            "*"
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests: x-request-id header
    // -----------------------------------------------------------------------

    /// Successful response includes x-request-id matching oxllm-[0-9a-f]{16}.
    #[tokio::test]
    async fn test_integration_request_id_on_success() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"Hello from prov"}}]}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .json(&serde_json::json!({
                "model": "test",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 200);
        let request_id = res
            .headers()
            .get("x-request-id")
            .expect("x-request-id header should be present");
        let id_str = request_id.to_str().unwrap();
        assert!(
            id_str.starts_with("oxllm-"),
            "x-request-id should start with 'oxllm-', got: {}",
            id_str
        );
        assert_eq!(
            id_str.len(),
            22,
            "x-request-id should be 'oxllm-' + 16 hex chars (len 22), got len {}",
            id_str.len()
        );
        // Verify remaining chars are valid hex
        let hex_part = &id_str[6..];
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "x-request-id hex part should be all hex chars, got: {}",
            hex_part
        );
    }

    /// Error response also includes x-request-id header.
    #[tokio::test]
    async fn test_integration_request_id_on_error() {
        let ok_response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 32\r\n\r\n{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}]}".to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(ok_response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let (_app_state, rs) = build_test_state(addr);
        let router = build_test_router(rs);
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                proxy_listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = reqwest::Client::new();
        let res = client
            .post(format!("http://{}/v1/chat/completions", proxy_addr))
            .body("not valid json")
            .header("Content-Type", "application/json")
            .send()
            .await
            .unwrap();

        assert_eq!(res.status(), 400);
        let request_id = res
            .headers()
            .get("x-request-id")
            .expect("x-request-id header should be present on error responses");
        let id_str = request_id.to_str().unwrap();
        assert!(
            id_str.starts_with("oxllm-"),
            "x-request-id should start with 'oxllm-', got: {}",
            id_str
        );
        assert_eq!(id_str.len(), 22);
    }
}
