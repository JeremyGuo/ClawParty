use crate::channel::{Channel, IncomingMessage};
use crate::config::WebChannelConfig;
use crate::domain::{ChannelAddress, OutgoingAttachment, OutgoingMessage, ProcessingState};
use agent_frame::SessionEvent;
use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{
    Router,
    extract::{Path, State, WebSocketUpgrade, ws},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, mpsc};

// ── Shared state ────────────────────────────────────────────────

/// Shared state between the axum handlers and the Channel trait impl.
struct WebChannelInner {
    id: String,
    listen_addr: String,
    auth_token: Option<String>,
    /// Sender half for incoming user messages → Server run loop.
    incoming_sender: RwLock<Option<mpsc::Sender<IncomingMessage>>>,
    /// Broadcast channel for real-time outgoing events → WS clients.
    event_bus: broadcast::Sender<WebSocketEvent>,
    /// Track last outgoing message per address for HTTP polling fallback.
    last_messages: RwLock<HashMap<String, Vec<OutgoingMessage>>>,
}

pub struct WebChannel {
    inner: Arc<WebChannelInner>,
}

impl WebChannel {
    pub fn from_config(config: WebChannelConfig) -> Result<Self> {
        let auth_token = match config.auth_token {
            Some(token) if !token.trim().is_empty() => Some(token),
            _ => std::env::var(&config.auth_token_env)
                .ok()
                .filter(|t| !t.trim().is_empty()),
        };
        let (event_tx, _) = broadcast::channel(256);
        Ok(Self {
            inner: Arc::new(WebChannelInner {
                id: config.id,
                listen_addr: config.listen_addr,
                auth_token,
                incoming_sender: RwLock::new(None),
                event_bus: event_tx,
                last_messages: RwLock::new(HashMap::new()),
            }),
        })
    }
}

// ── Channel trait implementation ────────────────────────────────

#[async_trait]
impl Channel for WebChannel {
    fn id(&self) -> &str {
        &self.inner.id
    }

    async fn run(self: Arc<Self>, sender: mpsc::Sender<IncomingMessage>) -> Result<()> {
        // Store the incoming sender for later use by HTTP handlers.
        {
            let mut guard = self.inner.incoming_sender.write().await;
            *guard = Some(sender);
        }

        let listen_addr = self.inner.listen_addr.clone();
        tracing::info!(
            log_stream = "channel",
            log_key = %self.inner.id,
            kind = "web_channel_starting",
            listen_addr = %listen_addr,
            has_auth = self.inner.auth_token.is_some(),
            "web channel starting"
        );

        let app = build_router(self.inner.clone());

        let listener = tokio::net::TcpListener::bind(&listen_addr)
            .await
            .with_context(|| format!("failed to bind web channel to {}", listen_addr))?;
        tracing::info!(
            log_stream = "channel",
            log_key = %self.inner.id,
            kind = "web_channel_listening",
            listen_addr = %listen_addr,
            "web channel listening"
        );
        axum::serve(listener, app)
            .await
            .context("web channel server error")?;
        Ok(())
    }

    async fn send(&self, address: &ChannelAddress, message: OutgoingMessage) -> Result<()> {
        // Broadcast to WS clients.
        let event = WebSocketEvent::OutgoingMessage {
            conversation_key: address.session_key(),
            text: message.text.clone().unwrap_or_default(),
            attachments: message.attachments.len() + message.images.len(),
        };
        let _ = self.inner.event_bus.send(event);

        // Also buffer for HTTP polling.
        let key = address.session_key();
        let mut guard = self.inner.last_messages.write().await;
        guard.entry(key).or_default().push(message);
        Ok(())
    }

    async fn send_media_group(
        &self,
        address: &ChannelAddress,
        images: Vec<OutgoingAttachment>,
    ) -> Result<()> {
        let event = WebSocketEvent::MediaGroup {
            conversation_key: address.session_key(),
            count: images.len(),
        };
        let _ = self.inner.event_bus.send(event);
        Ok(())
    }

    async fn set_processing(
        &self,
        address: &ChannelAddress,
        state: ProcessingState,
    ) -> Result<()> {
        let event = WebSocketEvent::Processing {
            conversation_key: address.session_key(),
            state: format!("{:?}", state),
        };
        let _ = self.inner.event_bus.send(event);
        Ok(())
    }

    async fn on_session_event(&self, address: &ChannelAddress, event: &SessionEvent) {
        let ws_event = WebSocketEvent::SessionEvent {
            conversation_key: address.session_key(),
            event_summary: summarize_session_event(event),
        };
        let _ = self.inner.event_bus.send(ws_event);
    }
}

// ── WebSocket event types ───────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
enum WebSocketEvent {
    #[serde(rename = "outgoing_message")]
    OutgoingMessage {
        conversation_key: String,
        text: String,
        attachments: usize,
    },
    #[serde(rename = "media_group")]
    MediaGroup {
        conversation_key: String,
        count: usize,
    },
    #[serde(rename = "processing")]
    Processing {
        conversation_key: String,
        state: String,
    },
    #[serde(rename = "session_event")]
    SessionEvent {
        conversation_key: String,
        event_summary: String,
    },
}

fn summarize_session_event(event: &SessionEvent) -> String {
    match event {
        SessionEvent::UserMessageReceived { text, .. } => {
            format!(
                "user_message: {}",
                text.as_deref().unwrap_or("(no text)").chars().take(80).collect::<String>()
            )
        }
        SessionEvent::ModelCallCompleted {
            round_index,
            tool_call_count,
            ..
        } => format!("model_call: round={round_index} tools={tool_call_count}"),
        SessionEvent::ToolCallCompleted {
            tool_name,
            errored,
            ..
        } => format!("tool_result: {tool_name} errored={errored}"),
        SessionEvent::CompactionCompleted { compacted, .. } => {
            format!("compaction: compacted={compacted}")
        }
        _ => format!("{:?}", std::mem::discriminant(event)),
    }
}

// ── Axum router ─────────────────────────────────────────────────

fn build_router(state: Arc<WebChannelInner>) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/api/health", get(health))
        .route("/api/send", post(send_message))
        .route("/ws", get(ws_handler))
        // Serve static assets from the embedded SPA.
        .route("/assets/{*path}", get(serve_static))
        .with_state(state)
}

// ── Auth helper ─────────────────────────────────────────────────

fn check_auth(state: &WebChannelInner, headers: &HeaderMap) -> Result<(), StatusCode> {
    let Some(expected_token) = &state.auth_token else {
        // No auth configured — open channel.
        return Ok(());
    };
    let auth_header = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match auth_header {
        Some(value) if value.strip_prefix("Bearer ").unwrap_or("") == expected_token => Ok(()),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

// ── HTTP handlers ───────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[derive(Deserialize)]
struct SendMessageRequest {
    text: String,
    /// Optional conversation key. If absent, uses a default.
    conversation_key: Option<String>,
}

async fn send_message(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    check_auth(&state, &headers)?;

    let sender_guard = state.incoming_sender.read().await;
    let Some(sender) = sender_guard.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let conversation_key = body
        .conversation_key
        .unwrap_or_else(|| "web-default".to_string());
    let address = ChannelAddress {
        channel_id: state.id.clone(),
        conversation_id: conversation_key.clone(),
        user_id: Some("web-user".to_string()),
        display_name: Some("Web User".to_string()),
    };

    let incoming = IncomingMessage {
        remote_message_id: uuid::Uuid::new_v4().to_string(),
        address,
        text: Some(body.text),
        attachments: Vec::new(),
        stored_attachments: Vec::new(),
        control: None,
    };
    sender
        .send(incoming)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({ "status": "sent" })))
}

// ── WebSocket handler ───────────────────────────────────────────

async fn ws_handler(
    State(state): State<Arc<WebChannelInner>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    check_auth(&state, &headers)?;
    Ok(ws.on_upgrade(move |socket| handle_ws(socket, state)))
}

async fn handle_ws(mut socket: ws::WebSocket, state: Arc<WebChannelInner>) {
    let mut rx = state.event_bus.subscribe();
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        if let Ok(json) = serde_json::to_string(&event) {
                            if socket.send(ws::Message::Text(json.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            log_stream = "channel",
                            kind = "ws_lagged",
                            lagged_count = n,
                            "WebSocket client lagged behind"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(ws::Message::Ping(payload))) => {
                        let _ = socket.send(ws::Message::Pong(payload)).await;
                    }
                    Some(Ok(ws::Message::Close(_))) | None => break,
                    _ => {} // Ignore text/binary from client for now.
                }
            }
        }
    }
}

// ── Static file serving ─────────────────────────────────────────

const INDEX_HTML: &str = include_str!("web_static/index.html");

async fn serve_index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn serve_static(Path(path): Path<String>) -> Response {
    // Minimal embedded static serving for a few known asset types.
    // In production, consider a CDN or a reverse proxy.
    let content = match path.as_str() {
        "app.js" => Some((include_str!("web_static/app.js"), "application/javascript")),
        "style.css" => Some((include_str!("web_static/style.css"), "text/css")),
        _ => None,
    };
    match content {
        Some((body, mime)) => ([(header::CONTENT_TYPE, mime)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
