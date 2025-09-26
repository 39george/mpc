use serde::{Deserialize, Serialize};

use crate::AuxMsg;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message<M> {
    pub session_id: String,
    pub data: Data<M>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Data<M> {
    StartCmd { i: u16, n: u16 },
    Incoming(Incoming<M>),
    Outgoing(Outgoing<M>),
}

impl std::fmt::Display for Data<AuxMsg> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Data::StartCmd { i, n } => {
                f.write_fmt(format_args!("set parity idx, i: {i}, n: {n}"))
            }
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

impl From<cggmp24::round_based::Outgoing<AuxMsg>> for Outgoing<AuxMsg> {
    fn from(value: cggmp24::round_based::Outgoing<AuxMsg>) -> Self {
        Self {
            recipient: value.recipient.into(),
            msg: value.msg,
        }
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
impl From<Incoming<AuxMsg>> for cggmp24::round_based::Incoming<AuxMsg> {
    fn from(value: Incoming<AuxMsg>) -> Self {
        Self {
            id: value.id,
            sender: value.sender,
            msg: value.msg,
            msg_type: value.msg_type.into(),
        }
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
