use anyhow::Context;
use anyhow::anyhow;
use axum::extract::Path;
use axum::{Json, extract::State};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::mpsc::unbounded;
use http::HeaderMap;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::manager::error::ErrorResponse;
use crate::manager::error::Result;
use crate::protocol;

// SessionManager — это сердце оркестратора
pub struct SessionManager {
    pub start_cmd: broadcast::Sender<()>,
    // Карта для маршрутизации сообщений между участниками сессии
    // Key: PartyIndex (0..N-1)
    // Value: Sender (канал для отправки MPC сообщений участнику)
    pub party_channels:
        HashMap<u16, UnboundedSender<protocol::Incoming<Vec<u8>>>>,
    // Дополнительные метаданные сессии: N, T, ExecutionId и т.д.
    pub metadata: SessionMetadata,
    // Можно добавить стейт-машину для отслеживания прогресса DKG
}

#[derive(Serialize, Deserialize)]
pub enum SessionStatus {
    Pending,
    Running,
    Done,
    Error,
}

pub struct SessionMetadata {
    pub participants: Vec<Uuid>,
    // КЛЮЧЕВОЕ ПОЛЕ
    pub session_type: protocol::SessionType,
    pub tag: String,
    pub status: SessionStatus,
}

// POST /api/v1/session/start
#[derive(Deserialize, Serialize)]
pub struct StartSessionRequest {
    pub tag: String,
    // КЛЮЧЕВОЕ ПОЛЕ
    pub session_type: protocol::SessionType,
    pub participants: HashSet<Uuid>,
}

// ДЛЯ КООРДИНАТОРА
// POST /api/v1/session
// Returns SessionId
#[tracing::instrument(name = "create session", skip_all)]
pub async fn create_session(
    State(state): State<super::AppState>,
    Json(req): Json<StartSessionRequest>,
) -> Result<Json<serde_json::Value>> {
    // Get session id
    let mut sorted_participants =
        req.participants.clone().into_iter().collect::<Vec<_>>();
    sorted_participants.sort();
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&sorted_participants).unwrap());
    hasher.update(serde_json::to_vec(&req.session_type).unwrap());
    // Если KeyGen/Signing, добавить T
    if let protocol::SessionType::KeyGen { threshold } = req.session_type {
        hasher.update(&threshold.to_le_bytes());
    }
    let session_id = hex::encode(hasher.finalize().to_vec());

    let (start_cmd_tx, _) = broadcast::channel::<()>(1);

    state.sessions.lock().await.insert(
        session_id.clone(),
        SessionManager {
            party_channels: Default::default(),
            metadata: SessionMetadata {
                participants: sorted_participants,
                session_type: req.session_type,
                tag: req.tag,
                status: SessionStatus::Pending,
            },
            start_cmd: start_cmd_tx,
        },
    );
    Ok(Json(json!({"session_id": session_id})))
}

// ДЛЯ УЧАСТНИКА
// GET /api/v1/sessions
#[tracing::instrument(name = "available sessions", skip_all)]
pub async fn available_sessions(
    State(state): State<super::AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>> {
    let signer_id: Uuid = headers
        .get("SIGNER-ID")
        .ok_or(ErrorResponse::Unauthorized(anyhow!("no signer-id")))?
        .to_str()
        .context("can't repr SIGNER-ID as string")
        .map_err(ErrorResponse::BadRequest)?
        .parse()
        .context("can't parse uuid")
        .map_err(ErrorResponse::BadRequest)?;
    let available = state
        .sessions
        .lock()
        .await
        .iter()
        .filter_map(|(k, v)| {
            if v.metadata.participants.contains(&signer_id) {
                Some(json!({"session_id": k, "tag": v.metadata.tag, "status": v.metadata.status } ))
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    Ok(Json(available))
}

// ДЛЯ УЧАСТНИКА
// POST /api/v1/session/start HTTP
// Начинает новую MPC сессию (KeyGen или Signing).
// Принимает N (количество участников), T (порог), session_type. Возвращает SessionID и PartyIndex для клиента.
// Оркестратор должен обрабатывать запрос запрос на присоединение или создание сессии для конкретной группы
#[tracing::instrument(name = "connect session", skip(state, ws))]
pub async fn connect_session(
    State(state): State<super::AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    ws: axum::extract::WebSocketUpgrade,
) -> Result<axum::response::Response> {
    let signer_id: Uuid = headers
        .get("SIGNER-ID")
        .ok_or(ErrorResponse::Unauthorized(anyhow!("no signer-id")))?
        .to_str()
        .context("can't repr SIGNER-ID as string")
        .map_err(ErrorResponse::BadRequest)?
        .parse()
        .context("can't parse uuid")
        .map_err(ErrorResponse::BadRequest)?;
    let mut session_lock = state.sessions.lock().await;
    let session_manager = session_lock
        .get_mut(&session_id)
        .context("no session found")
        .map_err(ErrorResponse::NotFound)?;
    let party_idx = session_manager
        .metadata
        .participants
        .iter()
        .position(|id| id == &signer_id) // <-- Ищем позицию SignerID
        .context("Signer is not a participant in this session")
        .map_err(ErrorResponse::NotFound)? as u16;

    // Проверка на повторное подключение (по Party Index)
    if session_manager.party_channels.contains_key(&party_idx) {
        return Err(ErrorResponse::Conflict(anyhow!(
            "Party index already connected"
        )));
    }

    let (in_tx, in_rx) = unbounded();
    session_manager.party_channels.insert(party_idx, in_tx);
    let n = session_manager.metadata.participants.len();
    let start_cmd_rx = session_manager.start_cmd.subscribe();
    let session_type = session_manager.metadata.session_type;

    drop(session_lock);
    Ok(ws.on_upgrade(move |socket| {
        handle_ws(
            state,
            socket,
            in_rx,
            start_cmd_rx,
            party_idx,
            n as u16,
            session_type,
            session_id,
        )
    }))
}

// ДЛЯ КООРДИНАТОРА
// POST /api/v1/session/{session_id}/start HTTP
// Начинает новую MPC сессию (KeyGen или Signing).
#[tracing::instrument(name = "connect session", skip(state))]
pub async fn start_session(
    State(state): State<super::AppState>,
    Path(session_id): Path<String>,
) -> Result<StatusCode> {
    let mut session_lock = state.sessions.lock().await;
    let session_manager = session_lock
        .get_mut(&session_id)
        .context("no session found")
        .map_err(ErrorResponse::NotFound)?;
    if let protocol::SessionType::KeyGen { threshold } =
        session_manager.metadata.session_type
        && session_manager.party_channels.len() < threshold as usize
    {
        return Err(ErrorResponse::Conflict(anyhow!(
            "won't run session, not enough participants for threshold"
        )));
    } else if session_manager.party_channels.len()
        != session_manager.metadata.participants.len()
    {
        return Err(ErrorResponse::Conflict(anyhow!(
            "won't run session, not all participants connected"
        )));
    }
    session_manager
        .start_cmd
        .send(())
        .context("failed to send start cmd")?;
    session_manager.metadata.status = SessionStatus::Running;
    Ok(StatusCode::OK)
}

// GET /ws/connect/:session_id/:party_id WebSocket
// Точка входа для всех участников. Устанавливает постоянное двустороннее соединение для обмена MPC сообщениями.
pub async fn connect_ws() -> StatusCode {
    StatusCode::OK
}

// GET /api/v1/session/:session_id/status HTTP
// Позволяет клиенту проверить текущий статус сессии (например, Pending, Running, Done, Error).
pub async fn session_status() -> StatusCode {
    StatusCode::OK
}

async fn handle_ws(
    state: super::AppState,
    mut ws: axum::extract::ws::WebSocket,
    mut in_rx: UnboundedReceiver<protocol::Incoming<Vec<u8>>>,
    mut start_cmd: broadcast::Receiver<()>,
    party_idx: u16,
    n: u16,
    session_type: protocol::SessionType,
    session_id: String,
) {
    tokio::spawn(async move {
        let msg = serde_json::to_vec(&protocol::Message {
            data: protocol::Data::<Vec<u8>>::StartCmd {
                i: party_idx,
                n,
                session_type,
            },
            session_id: session_id.clone(),
        })?;

        start_cmd.recv().await?;
        ws.send(axum::extract::ws::Message::binary(msg)).await?;

        let mut next_id: u64 = 0;
        // TODO: можно прокинуть ошибку из цикла из функции
        loop {
            tokio::select! {
                Some(incoming) = in_rx.next() => {
                    let msg = protocol::Message {session_id: session_id.clone(), data: protocol::Data::Incoming(incoming)};
                    let data = serde_json::to_vec(&msg).unwrap();
                    ws.send(axum::extract::ws::Message::binary(data)).await?;
                }
                Some(frame) = ws.next() => {
                    match frame {
                        Ok(axum::extract::ws::Message::Binary(bytes)) => {
                            let m: protocol::Message<Vec<u8>> = serde_json::from_slice(&bytes).unwrap();
                            tracing::info!("Читаем фрейм");
                            match m.data {
                                protocol::Data::Outgoing(protocol::Outgoing { recipient, msg }) => {
                                    match recipient {
                                        protocol::MessageDestination::OneParty(idx) => {
                                            tracing::info!("Отправляем в {idx}");
                                            let lock = state.sessions.lock().await;
                                            let Some(session) = lock.get(&m.session_id) else {
                                                continue;
                                            };
                                            let Some(peer_in) = session.party_channels.get(&idx) else {
                                                continue;
                                            };
                                            let id = {
                                                let x = next_id;
                                                next_id += 1;
                                                x
                                            };
                                            let incoming = protocol::Incoming {
                                                id,                                   // Идентификатор
                                                sender: party_idx,                    // От кого
                                                msg: msg,                             // Что
                                                msg_type: protocol::MessageType::P2P, // Тип
                                            };
                                            let _ = peer_in.unbounded_send(incoming);    // Отправляем в in_tx участника 2
                                        }
                                        protocol::MessageDestination::AllParties => {
                                            tracing::info!("Отправляем всем");
                                            let lock = state.sessions.lock().await;
                                            let Some(session) = lock.get(&m.session_id) else {
                                                continue;
                                            };
                                            for (&idx, peer_in) in &session.party_channels {
                                                if idx == party_idx {
                                                    continue;
                                                }
                                                let id = {
                                                    let x = next_id;
                                                    next_id += 1;
                                                    x
                                                };
                                                let incoming = protocol::Incoming {
                                                    sender: party_idx,
                                                    msg: msg.clone(), // clone для рассылки
                                                    id,
                                                    msg_type: protocol::MessageType::Broadcast,
                                                };
                                                let _ = peer_in.unbounded_send(incoming);
                                            }
                                        }
                                    }
                                },
                                _ => ()
                            }
                        }
                        Ok(axum::extract::ws::Message::Close(f)) => {
                            tracing::error!("Выходим без ошибки: {f:?}");
                            break;
                        }
                        Ok(m) => {
                            tracing::error!("Скипаем без ошибки: {m:?}");
                        }
                        Err(e) => {
                            tracing::error!("Выходим: {e}");
                            break;
                        },
                    }
                }
            }
        }

        // БЛОК ОЧИСТКИ (Выполняется независимо от того, как завершился async-блок выше)
        tracing::info!(
            "Cleaning up connection for Party {} in Session {}",
            party_idx,
            session_id
        );
        // Получаем блокировку, удаляем канал участника
        state.sessions.lock().await.get_mut(&session_id).map(
            |session_manager| {
                session_manager.party_channels.remove(&party_idx);
            },
        );
        tracing::info!("end to serve ws");
        Ok::<_, anyhow::Error>(())
    });
}
