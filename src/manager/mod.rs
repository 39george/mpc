use std::{collections::HashMap, sync::Arc};

use axum::routing;
use tokio::{net::TcpListener, sync::Mutex};

pub mod error;
pub mod handlers;

#[derive(Clone)]
pub struct AppState {
    _http_client: reqwest::Client,
    // Главное хранилище всех активных MPC сессий
    // Key: SessionID
    // Value: Структура, управляющая каналами и состоянием сессии
    sessions: Arc<Mutex<HashMap<String, handlers::SessionManager>>>,
}

type Server = axum::serve::Serve<
    tokio::net::TcpListener,
    axum::routing::IntoMakeService<axum::Router>,
    axum::Router,
>;

pub struct Application {
    _port: u16,
    server: Server,
}

impl Application {
    pub async fn build_server() -> anyhow::Result<Self> {
        let port = 10000;
        let addr = format!("127.0.0.1:{}", port);
        let listener = TcpListener::bind(addr).await?;

        let app_state = AppState {
            _http_client: reqwest::Client::new(),
            sessions: Default::default(),
        };

        let cors = tower_http::cors::CorsLayer::new()
            // allow `GET` and `POST` when accessing the resource
            .allow_methods([http::Method::GET, http::Method::POST])
            // allow requests from any origin
            .allow_origin(tower_http::cors::Any);

        let app = axum::Router::new()
            .route("/api/v1/session", routing::post(handlers::create_session))
            .route(
                "/api/v1/sessions",
                routing::get(handlers::available_sessions),
            )
            .route(
                "/api/v1/session/{session_id}/connect",
                routing::any(handlers::connect_session),
            )
            .route(
                "/api/v1/session/{session_id}/start",
                routing::post(handlers::start_session),
            )
            .route(
                "/ws/connect/{session_id}/{party_id}",
                routing::get(handlers::connect_ws),
            )
            .route(
                "/api/v1/session/{session_id}/status",
                routing::get(handlers::session_status),
            )
            .with_state(app_state)
            .layer(cors);

        let server = axum::serve(listener, app.into_make_service());
        Ok(Self {
            _port: port,
            server,
        })
    }
    pub async fn run_until_stopped(self) -> Result<(), std::io::Error> {
        self.server
            .with_graceful_shutdown(async {
                let ctrl_c = async {
                    tokio::signal::ctrl_c()
                        .await
                        .expect("failed to install Ctrl+C handler");
                };
                let terminate = async {
                    tokio::signal::unix::signal(
                        tokio::signal::unix::SignalKind::terminate(),
                    )
                    .expect("failed to install signal handler")
                    .recv()
                    .await;
                };
                tokio::select! {
                    () = ctrl_c => {},
                    () = terminate => {},
                }
                tracing::info!("Terminate signal received");
            })
            .await?;
        Ok(())
    }
}
