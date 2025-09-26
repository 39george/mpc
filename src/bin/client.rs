use anyhow::{Context, anyhow};
use cggmp24::KeyShare;
use cggmp24::key_share::{DirtyAuxInfo, Valid};
use cggmp24::progress::Stderr;
use cggmp24::supported_curves::Secp256k1;
use cggmp24::{ExecutionId, PregeneratedPrimes, round_based::MpcParty};
use clap::Parser;
use futures::SinkExt;
use futures::{
    StreamExt,
    channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded},
};
use http::header::{CONNECTION, UPGRADE};
use mpc::{AuxMsg, Mid, protocol};
use rand::rngs::OsRng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::future::Future;
use tokio::net::TcpStream;
use tokio::task::spawn_blocking;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{Instrument, Level};
use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Уникальный ID участника (UUID)
    #[arg(long)]
    signer_id: Uuid,

    /// Адрес сервера-оркестратора
    #[arg(short, long, default_value = "localhost:10000")]
    addr: String,

    /// ID сессии, к которой нужно подключиться
    #[arg(long)]
    session_id: String,
}

/// Эта функция — сердце транспортного уровня клиента.
/// Она владеет WebSocket соединением и связывает его с каналами для MPC протокола.
fn spawn_transport_handler<
    M: Serialize + DeserializeOwned + 'static + Send + Sync,
>(
    mut ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    // Канал для отправки сообщений, пришедших из сети, в MPC протокол
    in_tx: UnboundedSender<cggmp24::round_based::Incoming<M>>,
    // Канал для получения сообщений от MPC протокола для отправки в сеть
    mut out_rx: UnboundedReceiver<cggmp24::round_based::Outgoing<M>>,
    session_id: String,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                // 1. Получаем сообщение от нашего MPC протокола, чтобы отправить его на сервер
                Some(out_msg) = out_rx.next() => {
                    // Оборачиваем в наш кастомный протокол
                    let message_to_server = protocol::Message {
                        session_id: session_id.clone(),
                        data: protocol::Data::Outgoing(protocol::Outgoing::<Vec<u8>>::try_from(out_msg).unwrap()),
                    };
                    let payload = serde_json::to_vec(&message_to_server).unwrap();
                    if ws.send(Message::binary(payload)).await.is_err() {
                        tracing::error!("Failed to send message to server");
                        break;
                    }
                }

                // 2. Получаем сообщение с сервера для нашего MPC протокола
                Some(frame_result) = ws.next() => {
                    match frame_result {
                        Ok(Message::Binary(bytes)) => {
                            // Десериализуем сообщение от сервера
                            let Ok(msg) = serde_json::from_slice::<protocol::Message<Vec<u8>>>(&bytes) else {
                                let msg = String::from_utf8_lossy(&bytes);
                                tracing::error!("failed to parse message from server: {msg}");
                                continue;
                            };
                            // Если это входящее сообщение для MPC, отправляем его в канал
                            if let protocol::Data::Incoming(incoming) = msg.data {
                                if in_tx.unbounded_send(incoming.try_into().unwrap()).is_err() {
                                    tracing::error!("MPC protocol seems to be finished, exiting transport.");
                                    break;
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            tracing::info!("server closed connection");
                            break;
                        }
                        Ok(other) => {
                            tracing::info!("unexpected message: {other:?}");
                        }
                        Err(e) => {
                            tracing::error!("error reading from websocket: {}", e);
                            break;
                        }
                    }
                }
                // Если оба канала закрыты, выходим
                else => {
                    tracing::info!("Both websocket and outgoing channel are closed.");
                    break;
                }
            }
        }
    }.instrument(tracing::info_span!("transport_handler")));
}

// Вспомогательная функция для создания `MpcParty`
fn make_party<M: Serialize + DeserializeOwned + 'static + Send + Sync>(
    incoming: UnboundedReceiver<cggmp24::round_based::Incoming<M>>,
    outgoing: UnboundedSender<cggmp24::round_based::Outgoing<M>>,
) -> MpcParty<
    M,
    (
        impl futures::Stream<
            Item = Result<
                cggmp24::round_based::Incoming<M>,
                std::convert::Infallible,
            >,
        >,
        UnboundedSender<cggmp24::round_based::Outgoing<M>>,
    ),
> {
    MpcParty::connected((incoming.map(Ok), outgoing))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    setup_tracing();

    // --- 1. Подключение к серверу и получение параметров ---
    tracing::info!("Connecting to session {}", args.session_id);
    let mut ws =
        connect(&args.addr, &args.session_id, &args.signer_id.to_string())
            .await?;
    tracing::info!("WebSocket connection established");

    // Ожидаем первое сообщение от сервера с нашим индексом (i) и общим числом участников (n)
    let message: protocol::Message<AuxMsg> = match ws
        .next()
        .await
        .context("Failed to read initial message from server")??
    {
        Message::Binary(bytes) => serde_json::from_slice(&bytes)
            .context("Failed to parse initial message")?,
        other => {
            return Err(anyhow!("unexpected response: {other:?}"));
        }
    };

    let (i, n, session_type) = match message.data {
        protocol::Data::StartCmd { i, n, session_type } => (i, n, session_type),
        _ => {
            return Err(anyhow!(
                "Expected SetPartyIdx message, got something else"
            ));
        }
    };
    tracing::info!("Assigned party index i={i} out of n={n} participants");

    // --- 3. Запуск MPC протокола ---
    let eid = ExecutionId::new(args.session_id.as_bytes());

    // Пробуем загрузить заранее сгенерированные простые числа, чтобы не ждать каждый раз
    let preg = match read_preg(i) {
        Ok(p) => {
            tracing::info!(
                "Successfully loaded pregenerated primes for party {i}"
            );
            p
        }
        _ => {
            tracing::info!(
                "Pregenerated primes not found, generating new ones (this will take a while)..."
            );
            let preg = spawn_blocking(move || {
                PregeneratedPrimes::<Mid>::generate(&mut OsRng)
            })
            .await
            .unwrap();

            let preg_s = serde_json::to_string(&preg).unwrap();
            fs::write(format!("preg_{i}.json"), preg_s).unwrap();
            tracing::info!(
                "finished generating and saved primes for party {i}"
            );
            preg
        }
    };

    match session_type {
        protocol::SessionType::AuxInfoGen => {
            let (in_tx, in_rx) =
                unbounded::<cggmp24::round_based::Incoming<AuxMsg>>();
            let (out_tx, out_rx) =
                unbounded::<cggmp24::round_based::Outgoing<AuxMsg>>();
            spawn_transport_handler(ws, in_tx, out_rx, args.session_id.clone());
            let party = make_party(in_rx, out_tx);

            tracing::info!("starting AuxInfo generation protocol...");
            let aux_info_result = cggmp24::aux_info_gen(eid, i, n, preg)
                .set_progress_tracer(&mut Stderr::new())
                .start(&mut OsRng, party)
                .await;

            match aux_info_result {
                Ok(aux_info) => {
                    tracing::info!("AuxInfo generation successful!");
                    // В реальном приложении здесь нужно сохранить aux_info для будущего использования
                    let s = serde_json::to_string(&aux_info)?;
                    fs::write(format!("aux_info_{}.json", i), s)?;
                    tracing::info!("saved aux_info to aux_info_{}.json", i);
                }
                Err(e) => {
                    anyhow::bail!("AuxInfo generation failed: {:?}", e);
                }
            }
        }
        protocol::SessionType::KeyGen { threshold } => {
            let (in_tx, in_rx) = unbounded::<
                cggmp24::round_based::Incoming<mpc::ThresholdMsg>,
            >();
            let (out_tx, out_rx) = unbounded::<
                cggmp24::round_based::Outgoing<mpc::ThresholdMsg>,
            >();
            spawn_transport_handler(ws, in_tx, out_rx, args.session_id.clone());
            let party = make_party(in_rx, out_tx);

            let dirty = cggmp24::keygen::<Secp256k1>(eid, i, n)
                .set_threshold(threshold)
                .set_progress_tracer(&mut cggmp24::progress::Stderr::new())
                .start(&mut OsRng, party)
                .await?;
            let aux_info = read_aux_infos(i)?;
            let key_share = KeyShare::from_parts((dirty, aux_info))?;
            let pk = key_share.shared_public_key.to_bytes(false).to_vec(); // агрегированный secp256k1 pubkey
            let ta =
                tronic::domain::address::TronAddress::from_pk(&pk).unwrap();
            tracing::info!("tron addr: {}", ta);
        }
        protocol::SessionType::Signing => todo!(),
    }

    Ok(())
}

// --- Вспомогательные функции ---

fn read_preg(idx: u16) -> anyhow::Result<PregeneratedPrimes<Mid>> {
    let content = fs::read_to_string(format!("preg_{idx}.json"))?;
    let data = serde_json::from_str(&content)?;
    Ok(data)
}

fn read_aux_infos(idx: u16) -> anyhow::Result<Valid<DirtyAuxInfo<Mid>>> {
    let content = fs::read_to_string(format!("aux_info_{idx}.json"))?;
    let data = serde_json::from_str(&content)?;
    Ok(data)
}

#[tracing::instrument]
async fn connect(
    addr: &str,
    session_id: &str,
    signer_id: &str,
) -> anyhow::Result<WebSocketStream<MaybeTlsStream<TcpStream>>> {
    let url = format!("ws://{}/api/v1/session/{}/connect", addr, session_id);
    let request =
        tokio_tungstenite::tungstenite::handshake::client::Request::builder()
            .uri(&url)
            .method("GET")
            .header("Host", addr)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tokio_tungstenite::tungstenite::handshake::client::generate_key(
                ),
            )
            .header("SIGNER-ID", signer_id)
            .body(())?;

    let (ws, _) = connect_async(request).await?;
    Ok(ws)
}

fn setup_tracing() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_file(true)
        .with_line_number(true)
        .compact()
        .with_level(true);

    let env_layer = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(Level::INFO.into())
        .add_directive("axum::rejection=trace".parse().unwrap());

    let subscriber = tracing_subscriber::registry() // Use registry as base
        .with(fmt_layer)
        .with(env_layer);

    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set up tracing");
}
