use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::AuxMsg;

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub enum SessionType {
    // 1. Предварительная фаза. Требует N участников. Результат - кэшируемые AuxInfo.
    AuxInfoGen,
    // 2. Генерация общего секрета. Требует N участников и T. Результат - доля секрета.
    KeyGen { threshold: u16 },
    // 3. Подписание транзакции. Требует T из N участников.
    Signing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message<M> {
    pub session_id: String,
    pub data: Data<M>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Data<M> {
    StartCmd {
        i: u16,
        n: u16,
        session_type: SessionType,
    },
    Incoming(Incoming<M>),
    Outgoing(Outgoing<M>),
}

impl std::fmt::Display for Data<AuxMsg> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Data::StartCmd { i, n, session_type } => f.write_fmt(format_args!(
                "set parity idx, i: {i}, n: {n}, type: {session_type:?}"
            )),
            Data::Incoming(incoming) => f.write_fmt(format_args!(
                "Incoming, id: {}, sender: {}, type: {:?}",
                incoming.id, incoming.sender, incoming.msg_type
            )),
            Data::Outgoing(outgoing) => f.write_fmt(format_args!(
                "Outgoing, recipient: {:?}",
                outgoing.recipient
            )),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, Eq, PartialEq)]
pub enum MessageType {
    Broadcast,
    P2P,
}

/// Incoming message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Incoming<M> {
    pub id: u64,
    pub sender: u16,
    pub msg_type: MessageType,
    pub msg: M,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum MessageDestination {
    AllParties,
    OneParty(u16),
}

/// Outgoing message
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Outgoing<M> {
    /// Message destination: either one party (p2p message) or all parties (broadcast message)
    pub recipient: MessageDestination,
    /// Message being sent
    pub msg: M,
}

impl<M: Serialize> TryFrom<cggmp24::round_based::Outgoing<M>>
    for Outgoing<Vec<u8>>
{
    type Error = serde_json::Error;
    fn try_from(
        value: cggmp24::round_based::Outgoing<M>,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            recipient: value.recipient.into(),
            msg: serde_json::to_vec(&value.msg)?,
        })
    }
}
impl From<cggmp24::round_based::MessageDestination> for MessageDestination {
    fn from(value: cggmp24::round_based::MessageDestination) -> Self {
        match value {
            cggmp24::round_based::MessageDestination::AllParties => {
                Self::AllParties
            }
            cggmp24::round_based::MessageDestination::OneParty(i) => {
                Self::OneParty(i)
            }
        }
    }
}
impl<M: DeserializeOwned> TryFrom<Incoming<Vec<u8>>>
    for cggmp24::round_based::Incoming<M>
{
    type Error = serde_json::Error;
    fn try_from(value: Incoming<Vec<u8>>) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            sender: value.sender,
            msg: serde_json::from_slice(&value.msg)?,
            msg_type: value.msg_type.into(),
        })
    }
}
impl From<MessageType> for cggmp24::round_based::MessageType {
    fn from(value: MessageType) -> Self {
        match value {
            MessageType::Broadcast => Self::Broadcast,
            MessageType::P2P => Self::P2P,
        }
    }
}
