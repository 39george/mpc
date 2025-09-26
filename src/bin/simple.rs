use std::sync::Mutex;

use cggmp24::PregeneratedPrimes;
use cggmp24::Signature;
use cggmp24::key_share::DirtyAuxInfo;
use cggmp24::key_share::Valid;
use cggmp24::round_based::Incoming;
use cggmp24::round_based::MessageDestination;
use cggmp24::round_based::MessageType;
use cggmp24::round_based::MpcParty;
use cggmp24::round_based::Outgoing;
use cggmp24::security_level::define_security_level;
use cggmp24::supported_curves::Secp256k1;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use sha2::Sha256;
use signature::Verifier;
use tokio::task::spawn_blocking;
use tracing::Instrument;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;

type AuxMsg = cggmp24::key_refresh::msg::Msg<Sha256, Mid>;

#[derive(Clone)]
pub struct Std;
define_security_level!(Std {
    kappa_bits: 256,
    rsa_prime_bitlen: 1536,
    rsa_pubkey_bitlen: 3071,
    epsilon: 256 * 2,
    ell: 256,
    ell_prime: 256 * 5,
    m: 128,
});

#[derive(Clone)]
pub struct Mid;
define_security_level!(Mid {
    kappa_bits: 192,
    rsa_prime_bitlen: 1024,
    rsa_pubkey_bitlen: (1024 * 2) - 1, // 2*1024 - 1
    epsilon: 192 * 2,                  // ≈ 2*kappa
    ell: 192,
    ell_prime: 256 * 5, // 5*ell
    m: 128,
});

// ┌─────────────────────────────────────────────────────────────┐
// │                    Endpoint для участника i                 │
// │                                                             │
// │  ВНУТРЕННИЙ ИНТЕРФЕЙС (для MPC протокола)                   │
// │  ┌─────────────────┐        ┌─────────────────┐             │
// │  │     in_rx       │◄───────│     out_tx      │             │
// │  │ (Mutex<Option<  │        │   (копируемый)  │             │
// │  │ UnboundedRx>>)  │        │  UnboundedTx    │             │
// │  └─────────────────┘        └─────────────────┘             │
// │       ▲                              │                      │
// │       │                              ▼                      │
// │  MPC протокол                  MPC протокол                 │
// │  получает сообщения           отправляет сообщения          │
// │                                                             │
// │  ВНЕШНИЙ ИНТЕРФЕЙС (для роутера)                            │
// │  ┌─────────────────┐        ┌─────────────────┐             │
// │  │     in_tx       │───────►│     out_rx      │             │
// │  │   (копируемый)  │        │ (Mutex<Option<  │             │
// │  │  UnboundedTx    │        │ UnboundedRx>>)  │             │
// │  └─────────────────┘        └─────────────────┘             │
// └─────────────────────────────────────────────────────────────┘
struct Endpoint<M> {
    /// ВХОДная дверь. Роутер пишет сюда сообщения, предназначенные этому участнику
    in_tx: UnboundedSender<Incoming<M>>,
    /// ИСХОДной почтовый ящик. Роутер читает отсюда сообщения для маршрутизации
    out_rx: Mutex<Option<UnboundedReceiver<Outgoing<M>>>>,

    /// ВХОДной почтовый ящик. MPC протокол читает отсюда сообщения
    in_rx: Mutex<Option<UnboundedReceiver<Incoming<M>>>>,
    /// ИСХОДная дверь. MPC протокол пишет сюда сообщения для отправки другим
    out_tx: UnboundedSender<Outgoing<M>>,
}

fn new_endpoint<M>() -> Endpoint<M> {
    let (in_tx, in_rx) = unbounded::<Incoming<M>>();
    let (out_tx, out_rx) = unbounded::<Outgoing<M>>();
    Endpoint {
        in_tx,
        in_rx: Mutex::new(Some(in_rx)),
        out_tx,
        out_rx: Mutex::new(Some(out_rx)),
    }
}

// Участник 0 (my_index=0) хочет отправить сообщение участнику 2:
//
// ┌─────────────┐    out_tx.send()    ┌─────────────┐    peers_in.get(&2)   ┌─────────────┐
// │   MPC прот. │ ──────────────────► │   Роутер 0  │ ────────────────────► │   Роутер 2  │
// │  участника 0│                     │             │                       │             │
// └─────────────┘                     └─────────────┘                       └─────────────┘
//                                          │                                      │
//                                          ▼                                      ▼
//                                    out_rx.next().await                    in_tx.send(msg)
//                                                                                 │
//                                                                                 ▼
//                                                                         ┌─────────────┐
//                                                                         │   MPC прот. │
//                                                                         │  участника 2│
//                                                                         └─────────────┘
fn spawn_router<M: Clone + Send + Sync + 'static>(
    // Индекс ЭТОГО участника
    my_index: u16,
    // Исходящие сообщения ОТ этого участника
    mut out_rx: UnboundedReceiver<Outgoing<M>>,
    // Каналы К другим участникам
    peers_in: std::collections::HashMap<u16, UnboundedSender<Incoming<M>>>,
) {
    tokio::spawn(
        async move {
            let mut next_id: u64 = 0;
            while let Some(out) = out_rx.next().await {
                match out.recipient {
                    MessageDestination::OneParty(idx) => {
                        if let Some(peer_in) = peers_in.get(&idx) {
                            let id = {
                                let x = next_id;
                                next_id += 1;
                                x
                            };
                            let incoming = Incoming {
                                sender: my_index,           // От кого
                                msg: out.msg,               // Что
                                id,                         // Идентификатор
                                msg_type: MessageType::P2P, // Тип
                            };
                            let _ = peer_in.unbounded_send(incoming); // Отправляем в in_tx участника 2
                        }
                    }
                    MessageDestination::AllParties => {
                        for (&idx, peer_in) in &peers_in {
                            if idx == my_index {
                                continue;
                            }
                            let id = {
                                let x = next_id;
                                next_id += 1;
                                x
                            };
                            let incoming = Incoming {
                                sender: my_index,
                                msg: out.msg.clone(), // clone для рассылки
                                id,
                                msg_type: MessageType::Broadcast,
                            };
                            let _ = peer_in.unbounded_send(incoming);
                        }
                    }
                }
            }
        }
        .instrument(tracing::info_span!("spawning router")),
    );
}

fn make_party<M>(
    ep: &Endpoint<M>,
) -> MpcParty<
    M,
    (
        impl futures::Stream<Item = Result<Incoming<M>, std::convert::Infallible>>,
        UnboundedSender<Outgoing<M>>,
    ),
> {
    let incoming = ep
        .in_rx
        .lock()
        .unwrap()
        .take()
        .expect("party already in use")
        .map(Ok);
    let outgoing = ep.out_tx.clone();
    MpcParty::connected((incoming, outgoing))
}

fn setup_network<M: Clone + Send + Sync + 'static>(n: u16) -> Vec<Endpoint<M>> {
    let eps: Vec<Endpoint<M>> = (0..n).map(|_| new_endpoint()).collect();
    tracing::info!("created endpoints for {n}");
    for i in 0..n {
        let mut peers_in = std::collections::HashMap::new();
        for j in 0..n {
            if i == j {
                continue;
            }
            peers_in.insert(j, eps[j as usize].in_tx.clone());
        }
        let out_rx = eps[i as usize]
            .out_rx
            .lock()
            .unwrap()
            .take()
            .expect("router already started");
        spawn_router(i, out_rx, peers_in);
    }
    eps
}

/// === Демо: aux → keygen(t=2) → signing(b\"hello\") для 3 участников ===
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use cggmp24::{DataToSign, ExecutionId, KeyShare};
    use rand::rngs::OsRng;
    setup_tracing();

    let n: u16 = 4;

    // ---------- 1) AUX ----------
    let eps = setup_network::<AuxMsg>(n);
    let eid_aux = ExecutionId::new(b"aux-1");

    let aux_infos = match extract_aux_infos() {
        Ok(i) => i,
        _ => {
            futures::future::try_join_all((0..n).map(|i| {
                let party = make_party(&eps[i as usize]);
                async move {
                    let preg = match read_preg(i) {
                        Ok(p) => {
                            tracing::info!("found {i} primes");
                            p
                        }
                        _ => {
                            let preg = spawn_blocking(|| {
                                cggmp24::PregeneratedPrimes::<Mid>::generate(
                                    &mut OsRng,
                                )
                            })
                            .instrument(tracing::info_span!(
                                "generate primes",
                                idx = i
                            ))
                            .await
                            .unwrap();
                            let preg_s = serde_json::to_string(&preg).unwrap();
                            std::fs::write(format!("preg_{i}.json"), preg_s)
                                .unwrap();
                            preg
                        }
                    };
                    cggmp24::aux_info_gen(eid_aux, i, n, preg)
                        .set_progress_tracer(
                            &mut cggmp24::progress::Stderr::new(),
                        )
                        .start(&mut OsRng, party)
                        .await
                }
            }))
            .await?
        }
    };
    // let s = serde_json::to_string(&aux_infos).unwrap();
    // std::fs::write("aux_infos.json", s).unwrap();
    drop(eps); // удобнее пересобрать каналы на следующий протокол
    println!("Aux stage done\n\n");

    // ---------- 2) KEYGEN (t = 2) ----------
    let eps = setup_network(n);
    let eid_key = ExecutionId::new(b"keygen-1");

    let incomplete = futures::future::try_join_all((0..n).map(|i| {
        let party = make_party(&eps[i as usize]);
        async move {
            cggmp24::keygen::<Secp256k1>(eid_key, i, n)
                .set_threshold(2)
                .set_progress_tracer(&mut cggmp24::progress::Stderr::new())
                .start(&mut OsRng, party)
                .await
        }
    }))
    .await?;
    drop(eps);
    println!("Keygen stage done\n\n");

    // Собираем завершённые key shares
    let key_shares: Vec<_> = (0..n)
        .map(|i| {
            KeyShare::from_parts((
                incomplete[i as usize].clone(),
                aux_infos[i as usize].clone(),
            ))
        })
        .collect::<Result<_, _>>()?;
    for i in 0..n as usize {
        let pk = key_shares[i].shared_public_key.to_bytes(false).to_vec(); // агрегированный secp256k1 pubkey
        let ta = tronic::domain::address::TronAddress::from_pk(&pk).unwrap();
        println!("Tron addr: {}", ta);
    }
    let pk = key_shares[0].shared_public_key.to_bytes(false).to_vec(); // агрегированный secp256k1 pubkey

    for i in 0..n as usize {
        // 1. Получаем доступ к CoreSecretKeyShare (где хранится x_i)
        let core_share = &key_shares[i].core;

        // 2. Получаем SecretScalar (ключ x_i)
        // .x_i — это NonZero<SecretScalar<Secp256k1>>
        let secret_scalar: &cggmp24::generic_ec::Scalar<_> =
            core_share.x.as_ref();

        // 3. Преобразуем SecretScalar в байты и в шестнадцатеричную строку для печати
        let secret_bytes = secret_scalar.to_be_bytes();
        let secret_hex = hex::encode(secret_bytes);

        println!(
            "Участник {} (Индекс {}): Секретная доля (x_i): {}",
            i + 1,
            i,
            secret_hex
        );

        // Печатаем публичный ключ для сравнения (это остаток от вашего кода)
        let pk = key_shares[i].shared_public_key.to_bytes(false).to_vec();
        let ta = tronic::domain::address::TronAddress::from_pk(&pk).unwrap();
        println!("  -> Общий адрес Tron: {}\n", ta);
    }

    // ---------- 3) SIGNING (выбираем 2 из 3, например [0, 2]) ----------
    let signers = [0u16, 2u16];
    // Создаем сеть ТОЛЬКО для подписантов. Их двое.
    let eps = setup_network(signers.len() as u16);
    let eid_sign = ExecutionId::new(b"sign-hello");
    let data = DataToSign::digest::<Sha256>(b"hello");

    let (sig0, _sig2): (Signature<Secp256k1>, Signature<Secp256k1>) = tokio::try_join!(
        async {
            let i_local = 0u16; // Локальный индекс первого подписанта
            let i_global = signers[i_local as usize]; // Его глобальный индекс (будет 0)

            // Для сети eps используем ЛОКАЛЬНЫЙ индекс
            let party = make_party(&eps[i_local as usize]);

            cggmp24::signing(
                eid_sign,
                i_local,
                &signers,
                // А для доли ключа - ГЛОБАЛЬНЫЙ
                &key_shares[i_global as usize],
            )
            .sign(&mut OsRng, party, data)
            .await
        },
        async {
            let i_local = 1u16; // Локальный индекс второго подписанта
            let i_global = signers[i_local as usize]; // Его глобальный индекс (будет 2)

            // Для сети eps используем ЛОКАЛЬНЫЙ индекс
            let party = make_party(&eps[i_local as usize]);

            cggmp24::signing(
                eid_sign,
                i_local,
                &signers,
                // А для доли ключа - ГЛОБАЛЬНЫЙ
                &key_shares[i_global as usize],
            )
            .sign(&mut OsRng, party, data)
            .await
        }
    )?;
    println!("Signing stage done\n\n");

    let r = sig0.r.to_be_bytes().to_vec();
    let s = sig0.s.to_be_bytes().to_vec();
    let mut sig64 = [0u8; 64];
    sig64[..32].copy_from_slice(&r);
    sig64[32..].copy_from_slice(&s);
    println!(
        "✅ t=2-of-3 signature on \"hello\": r: {} s: {}",
        hex::encode(r),
        hex::encode(s)
    );
    let sig = k256::ecdsa::Signature::from_slice(&sig64).unwrap();
    let verifying_key = k256::ecdsa::VerifyingKey::from_sec1_bytes(&pk)?;
    verifying_key.verify(b"hello", &sig).unwrap();
    Ok(())
}

fn extract_aux_infos() -> anyhow::Result<Vec<Valid<DirtyAuxInfo<Mid>>>> {
    tracing::info!("extract aux infos");
    let file = std::fs::read_to_string("aux_infos.json")?;
    let data = serde_json::from_str::<Vec<Valid<DirtyAuxInfo<Mid>>>>(&file)?;
    Ok(data)
}

fn read_preg(idx: u16) -> anyhow::Result<PregeneratedPrimes<Mid>> {
    tracing::info!("read preg with idx {idx}");
    let file = std::fs::read_to_string(format!("preg_{idx}.json"))?;
    let data = serde_json::from_str(&file)?;
    Ok(data)
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
