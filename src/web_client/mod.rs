use std::{collections::HashMap, net::SocketAddr, path::PathBuf, time::SystemTime};

use axum::{
    extract::{
        connect_info::ConnectInfo,
        ws::{Message, WebSocket, WebSocketUpgrade},
        Json, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use hbb_common::{
    config::{self, Config, PeerConfig},
    log, tokio,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

const OPTION_ENABLED: &str = "web-client-enabled";
const OPTION_BIND: &str = "web-client-bind";
const OPTION_PORT: &str = "web-client-port";
const OPTION_PUBLIC_ACCESS: &str = "web-client-public-access";
const DEFAULT_BIND: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 21120;

#[derive(Clone)]
struct AppState {
    config: WebClientConfig,
}

#[derive(Clone, Debug, Serialize)]
pub struct WebClientConfig {
    pub enabled: bool,
    pub bind: String,
    pub port: u16,
    pub public_access: bool,
}

#[derive(Deserialize)]
struct LoginRequest {
    password: Option<String>,
}

#[derive(Deserialize)]
struct SetOptionsRequest {
    options: HashMap<String, String>,
}

#[derive(Deserialize)]
struct CreateSessionRequest {
    peer_id: String,
    mode: Option<String>,
}

#[derive(Serialize)]
struct BootstrapResponse {
    version: &'static str,
    platform: &'static str,
    access_mode: &'static str,
    password_set: bool,
    config: WebClientConfig,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_in: u64,
}

#[derive(Serialize)]
struct PeerEntry {
    id: String,
    username: String,
    hostname: String,
    platform: String,
    alias: String,
    note: String,
    modified_at: u64,
    password_saved: bool,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
    peer_id: String,
    mode: String,
    status: &'static str,
}

#[derive(Serialize, Deserialize)]
struct WsEnvelope {
    request_id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbaFrameHeader {
    pub session_id: u32,
    pub display: u16,
    pub width: u16,
    pub height: u16,
    pub format: u8,
    pub seq: u64,
}

impl WebClientConfig {
    pub fn load() -> Self {
        let bind = option_or_default(OPTION_BIND, DEFAULT_BIND);
        let port = Config::get_option(OPTION_PORT)
            .parse::<u16>()
            .unwrap_or(DEFAULT_PORT);
        Self {
            enabled: config::option2bool(OPTION_ENABLED, &Config::get_option(OPTION_ENABLED)),
            public_access: config::option2bool(
                OPTION_PUBLIC_ACCESS,
                &Config::get_option(OPTION_PUBLIC_ACCESS),
            ),
            bind,
            port,
        }
    }

    pub fn addr(&self) -> Result<SocketAddr, String> {
        format!("{}:{}", self.bind, self.port)
            .parse()
            .map_err(|err| format!("invalid web client listen addr: {err}"))
    }

    pub fn is_local_only(&self) -> bool {
        self.bind == "127.0.0.1" || self.bind == "::1" || self.bind == "localhost"
    }

    pub fn validate(&self) -> Result<(), String> {
        self.addr()?;
        if !self.is_local_only() && !Config::has_permanent_password() {
            return Err("web client public listen requires permanent password".to_owned());
        }
        Ok(())
    }
}

pub fn start_if_enabled() {
    let config = WebClientConfig::load();
    if !config.enabled {
        return;
    }
    if let Err(err) = config.validate() {
        log::error!("Failed to start web client: {}", err);
        return;
    }
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                log::error!("Failed to create web client runtime: {}", err);
                return;
            }
        };
        runtime.block_on(async move {
            if let Err(err) = serve(config).await {
                log::error!("Web client stopped: {}", err);
            }
        });
    });
}

async fn serve(config: WebClientConfig) -> Result<(), String> {
    let addr = config.addr()?;
    let state = AppState { config };
    let assets = web_assets_dir();
    let static_service = ServeDir::new(&assets).fallback(ServeFile::new(assets.join("index.html")));
    let app = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/bootstrap", get(bootstrap))
        .route("/api/options", get(get_options).post(set_options))
        .route("/api/peers", get(peers))
        .route("/api/sessions", post(create_session))
        .route("/ws", get(ws_handler))
        .fallback_service(static_service)
        .with_state(state);
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|err| err.to_string())?;
    log::info!("Web client listening on http://{}", addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .map_err(|err| err.to_string())
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginRequest>,
) -> Response {
    let local = is_local_addr(&addr);
    if !Config::has_permanent_password() {
        if local && state.config.is_local_only() {
            return Json(LoginResponse {
                token: new_session_token(),
                expires_in: 3600,
            })
            .into_response();
        }
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "password_required"})),
        )
            .into_response();
    }
    if req
        .password
        .as_deref()
        .map(Config::matches_permanent_password_plain)
        .unwrap_or(false)
    {
        Json(LoginResponse {
            token: new_session_token(),
            expires_in: 3600,
        })
        .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid_password"})),
        )
            .into_response()
    }
}

async fn bootstrap(State(state): State<AppState>) -> Json<BootstrapResponse> {
    Json(BootstrapResponse {
        version: crate::VERSION,
        platform: std::env::consts::OS,
        access_mode: if state.config.is_local_only() {
            "local"
        } else if state.config.public_access {
            "public"
        } else {
            "lan"
        },
        password_set: Config::has_permanent_password(),
        config: state.config,
    })
}

async fn get_options() -> Json<HashMap<String, String>> {
    Json(web_options())
}

async fn set_options(Json(req): Json<SetOptionsRequest>) -> Json<HashMap<String, String>> {
    for (key, value) in req.options {
        if is_web_option(&key) {
            Config::set_option(key, value);
        }
    }
    Json(web_options())
}

async fn peers() -> Json<Vec<PeerEntry>> {
    let peers = PeerConfig::peers(None)
        .into_iter()
        .map(|(id, modified_at, peer)| peer_entry(id, modified_at, peer))
        .collect();
    Json(peers)
}

async fn create_session(Json(req): Json<CreateSessionRequest>) -> Json<CreateSessionResponse> {
    let mode = req.mode.unwrap_or_else(|| "remote".to_owned());
    Json(CreateSessionResponse {
        session_id: Uuid::new_v4().to_string(),
        peer_id: req.peer_id,
        mode,
        status: "pending",
    })
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        match msg {
            Message::Text(text) => {
                let response = match serde_json::from_str::<WsEnvelope>(&text) {
                    Ok(envelope) => ws_ok(envelope.request_id, envelope.kind),
                    Err(err) => ws_error(None, err.to_string()),
                };
                if socket.send(Message::Text(response.into())).await.is_err() {
                    return;
                }
            }
            Message::Binary(data) => {
                let response = match parse_rgba_frame_header(&data) {
                    Ok(header) => json!({
                        "type": "ok",
                        "payload": {
                            "message": "frame.accepted",
                            "session_id": header.session_id,
                            "seq": header.seq
                        }
                    }),
                    Err(err) => ws_error(None, err),
                };
                if socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Message::Close(_) => return,
            _ => {}
        }
    }
}

pub fn parse_rgba_frame_header(data: &[u8]) -> Result<RgbaFrameHeader, String> {
    if data.len() < 21 {
        return Err("rgba frame header too short".to_owned());
    }
    Ok(RgbaFrameHeader {
        session_id: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        display: u16::from_le_bytes([data[4], data[5]]),
        width: u16::from_le_bytes([data[6], data[7]]),
        height: u16::from_le_bytes([data[8], data[9]]),
        format: data[10],
        seq: u64::from_le_bytes([
            data[11], data[12], data[13], data[14], data[15], data[16], data[17], data[18],
        ]),
    })
}

fn ws_ok(request_id: Option<String>, kind: String) -> String {
    json!({
        "type": "ok",
        "request_id": request_id,
        "payload": {
            "message": kind
        }
    })
    .to_string()
}

fn ws_error(request_id: Option<String>, error: String) -> Value {
    json!({
        "type": "error",
        "request_id": request_id,
        "payload": {
            "error": error
        }
    })
}

fn web_options() -> HashMap<String, String> {
    [
        OPTION_ENABLED,
        OPTION_BIND,
        OPTION_PORT,
        OPTION_PUBLIC_ACCESS,
    ]
    .into_iter()
    .map(|key| (key.to_owned(), option_or_default(key, default_option(key))))
    .collect()
}

fn is_web_option(key: &str) -> bool {
    matches!(
        key,
        OPTION_ENABLED | OPTION_BIND | OPTION_PORT | OPTION_PUBLIC_ACCESS
    )
}

fn option_or_default(key: &str, default: &str) -> String {
    let value = Config::get_option(key);
    if value.is_empty() {
        default.to_owned()
    } else {
        value
    }
}

fn default_option(key: &str) -> &'static str {
    match key {
        OPTION_BIND => DEFAULT_BIND,
        OPTION_PORT => "21120",
        _ => "",
    }
}

fn peer_entry(id: String, modified_at: SystemTime, peer: PeerConfig) -> PeerEntry {
    let modified_at = modified_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    PeerEntry {
        alias: peer.options.get("alias").cloned().unwrap_or_default(),
        note: peer.options.get("note").cloned().unwrap_or_default(),
        username: peer.info.username,
        hostname: peer.info.hostname,
        platform: peer.info.platform,
        password_saved: !peer.password.is_empty(),
        id,
        modified_at,
    }
}

fn is_local_addr(addr: &SocketAddr) -> bool {
    addr.ip().is_loopback()
}

fn new_session_token() -> String {
    Uuid::new_v4().to_string()
}

fn web_assets_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|path| path.join("ui").join("dist")))
        .filter(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from("ui/dist"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rgba_header() {
        let mut data = vec![0; 21];
        data[0..4].copy_from_slice(&7u32.to_le_bytes());
        data[4..6].copy_from_slice(&2u16.to_le_bytes());
        data[6..8].copy_from_slice(&1920u16.to_le_bytes());
        data[8..10].copy_from_slice(&1080u16.to_le_bytes());
        data[10] = 1;
        data[11..19].copy_from_slice(&42u64.to_le_bytes());
        let header = parse_rgba_frame_header(&data).unwrap();
        assert_eq!(header.session_id, 7);
        assert_eq!(header.display, 2);
        assert_eq!(header.width, 1920);
        assert_eq!(header.height, 1080);
        assert_eq!(header.format, 1);
        assert_eq!(header.seq, 42);
    }

    #[test]
    fn reject_short_rgba_header() {
        assert!(parse_rgba_frame_header(&[0; 20]).is_err());
    }
}
