use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
};
use tokio::sync::{mpsc, RwLock};
use tower_http::cors::{Any, CorsLayer};

// Message types for communication
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "register")]
    Register { user_id: String },
    #[serde(rename = "message")]
    Message { to: String, data: String, message_id: Option<String> },
    #[serde(rename = "typing")]
    Typing { to: String, is_typing: bool },
    #[serde(rename = "delivered")]
    Delivered { to: String, message_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "registered")]
    Registered { user_id: String },
    #[serde(rename = "message")]
    Message { from: String, data: String, message_id: Option<String> },
    #[serde(rename = "typing")]
    Typing { from: String, is_typing: bool },
    #[serde(rename = "delivered")]
    Delivered { from: String, message_id: String },
    #[serde(rename = "offline")]
    Offline { user_id: String },
    #[serde(rename = "error")]
    Error { message: String },
}

// Shared state for managing connections
type Connections = Arc<RwLock<HashMap<String, mpsc::UnboundedSender<String>>>>;

#[derive(Clone)]
struct AppState {
    connections: Connections,
}

#[tokio::main]
async fn main() {
    let state = AppState {
        connections: Arc::new(RwLock::new(HashMap::new())),
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/ws", get(ws_handler))
        .layer(cors)
        .with_state(state);

    // Use PORT from environment (for Render) or default to 8080
    let port = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    println!("🚀 HUMLOG Backend running on port {} (ws://0.0.0.0:{}/ws)", port, port);
    
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn health_check() -> &'static str {
    "OK"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    
    let mut user_id: Option<String> = None;

    // Task to send messages to the WebSocket
    let mut send_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Clone state for use in the async task
    let state_clone = state.clone();
    
    // Task to receive messages from the WebSocket
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Text(text))) = receiver.next().await {
            // Parse incoming message
            match serde_json::from_str::<ClientMessage>(&text) {
                Ok(ClientMessage::Register { user_id: uid }) => {
                    // Register the user
                    user_id = Some(uid.clone());
                    state_clone.connections.write().await.insert(uid.clone(), tx.clone());
                    
                    let response = ServerMessage::Registered { user_id: uid };
                    if let Ok(json) = serde_json::to_string(&response) {
                        let _ = tx.send(json);
                    }
                }
                Ok(ClientMessage::Message { to, data, message_id }) => {
                    // Forward message to recipient
                    let connections = state_clone.connections.read().await;
                    if let Some(recipient_tx) = connections.get(&to) {
                        if let Some(ref from) = user_id {
                            let response = ServerMessage::Message {
                                from: from.clone(),
                                data,
                                message_id: message_id.clone(),
                            };
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = recipient_tx.send(json);
                            }
                        }
                    } else {
                        // Recipient is offline
                        let response = ServerMessage::Offline { user_id: to };
                        if let Ok(json) = serde_json::to_string(&response) {
                            let _ = tx.send(json);
                        }
                    }
                }
                Ok(ClientMessage::Typing { to, is_typing }) => {
                    // Forward typing indicator to recipient
                    let connections = state_clone.connections.read().await;
                    if let Some(recipient_tx) = connections.get(&to) {
                        if let Some(ref from) = user_id {
                            let response = ServerMessage::Typing {
                                from: from.clone(),
                                is_typing,
                            };
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = recipient_tx.send(json);
                            }
                        }
                    }
                }
                Ok(ClientMessage::Delivered { to, message_id }) => {
                    // Forward delivery receipt to sender
                    let connections = state_clone.connections.read().await;
                    if let Some(recipient_tx) = connections.get(&to) {
                        if let Some(ref from) = user_id {
                            let response = ServerMessage::Delivered {
                                from: from.clone(),
                                message_id,
                            };
                            if let Ok(json) = serde_json::to_string(&response) {
                                let _ = recipient_tx.send(json);
                            }
                        }
                    }
                }
                Err(e) => {
                    let response = ServerMessage::Error {
                        message: format!("Invalid message format: {}", e),
                    };
                    if let Ok(json) = serde_json::to_string(&response) {
                        let _ = tx.send(json);
                    }
                }
            }
        }
        user_id
    });

    // Wait for either task to finish
    tokio::select! {
        uid = &mut recv_task => {
            send_task.abort();
            if let Ok(Some(uid)) = uid {
                // Remove user from connections
                state.connections.write().await.remove(&uid);
                println!("User {} disconnected", uid);
            }
        }
        _ = &mut send_task => {
            recv_task.abort();
        }
    }
}

