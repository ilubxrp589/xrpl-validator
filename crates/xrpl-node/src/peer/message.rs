//! Peer message types — wraps protobuf-generated structs into a unified enum.

use prost::Message as ProstMessage;

use super::protocol;
use crate::NodeError;

/// All message types in the XRPL peer protocol.
///
/// Each variant wraps the corresponding prost-generated struct.
/// The discriminant values match `MessageType` from xrpl.proto.
#[derive(Debug, Clone)]
pub enum PeerMessage {
    Manifests(protocol::TmManifests),
    Ping(protocol::TmPing),
    Cluster(protocol::TmCluster),
    Endpoints(protocol::TmEndpoints),
    Transaction(protocol::TmTransaction),
    GetLedger(protocol::TmGetLedger),
    LedgerData(protocol::TmLedgerData),
    ProposeSet(protocol::TmProposeSet),
    StatusChange(protocol::TmStatusChange),
    HaveTransactionSet(protocol::TmHaveTransactionSet),
    Validation(protocol::TmValidation),
    GetObjects(protocol::TmGetObjectByHash),
    ValidatorList(protocol::TmValidatorList),
    Squelch(protocol::TmSquelch),
    ValidatorListCollection(protocol::TmValidatorListCollection),
    ProofPathRequest(protocol::TmProofPathRequest),
    ProofPathResponse(protocol::TmProofPathResponse),
    ReplayDeltaRequest(protocol::TmReplayDeltaRequest),
    ReplayDeltaResponse(protocol::TmReplayDeltaResponse),
    HaveTransactions(protocol::TmHaveTransactions),
    Transactions(protocol::TmTransactions),
}

impl PeerMessage {
    /// The 2-byte message type code for the wire protocol.
    pub fn type_code(&self) -> u16 {
        match self {
            Self::Manifests(_) => 2,
            Self::Ping(_) => 3,
            Self::Cluster(_) => 5,
            Self::Endpoints(_) => 15,
            Self::Transaction(_) => 30,
            Self::GetLedger(_) => 31,
            Self::LedgerData(_) => 32,
            Self::ProposeSet(_) => 33,
            Self::StatusChange(_) => 34,
            Self::HaveTransactionSet(_) => 35,
            Self::Validation(_) => 41,
            Self::GetObjects(_) => 42,
            Self::ValidatorList(_) => 54,
            Self::Squelch(_) => 55,
            Self::ValidatorListCollection(_) => 56,
            Self::ProofPathRequest(_) => 57,
            Self::ProofPathResponse(_) => 58,
            Self::ReplayDeltaRequest(_) => 59,
            Self::ReplayDeltaResponse(_) => 60,
            Self::HaveTransactions(_) => 63,
            Self::Transactions(_) => 64,
        }
    }

    /// Serialize the inner protobuf message to bytes.
    pub fn encode_to_vec(&self) -> Vec<u8> {
        match self {
            Self::Manifests(m) => m.encode_to_vec(),
            Self::Ping(m) => m.encode_to_vec(),
            Self::Cluster(m) => m.encode_to_vec(),
            Self::Endpoints(m) => m.encode_to_vec(),
            Self::Transaction(m) => m.encode_to_vec(),
            Self::GetLedger(m) => m.encode_to_vec(),
            Self::LedgerData(m) => m.encode_to_vec(),
            Self::ProposeSet(m) => m.encode_to_vec(),
            Self::StatusChange(m) => m.encode_to_vec(),
            Self::HaveTransactionSet(m) => m.encode_to_vec(),
            Self::Validation(m) => m.encode_to_vec(),
            Self::GetObjects(m) => m.encode_to_vec(),
            Self::ValidatorList(m) => m.encode_to_vec(),
            Self::Squelch(m) => m.encode_to_vec(),
            Self::ValidatorListCollection(m) => m.encode_to_vec(),
            Self::ProofPathRequest(m) => m.encode_to_vec(),
            Self::ProofPathResponse(m) => m.encode_to_vec(),
            Self::ReplayDeltaRequest(m) => m.encode_to_vec(),
            Self::ReplayDeltaResponse(m) => m.encode_to_vec(),
            Self::HaveTransactions(m) => m.encode_to_vec(),
            Self::Transactions(m) => m.encode_to_vec(),
        }
    }

    /// Decode a protobuf message from a type code and payload bytes.
    pub fn decode(type_code: u16, data: &[u8]) -> Result<Self, NodeError> {
        let err = |e: prost::DecodeError| {
            NodeError::MessageDecode(format!("type {type_code}: {e}"))
        };

        match type_code {
            2 => Ok(Self::Manifests(protocol::TmManifests::decode(data).map_err(err)?)),
            3 => Ok(Self::Ping(protocol::TmPing::decode(data).map_err(err)?)),
            5 => Ok(Self::Cluster(protocol::TmCluster::decode(data).map_err(err)?)),
            15 => Ok(Self::Endpoints(protocol::TmEndpoints::decode(data).map_err(err)?)),
            30 => Ok(Self::Transaction(protocol::TmTransaction::decode(data).map_err(err)?)),
            31 => Ok(Self::GetLedger(protocol::TmGetLedger::decode(data).map_err(err)?)),
            32 => Ok(Self::LedgerData(protocol::TmLedgerData::decode(data).map_err(err)?)),
            33 => Ok(Self::ProposeSet(protocol::TmProposeSet::decode(data).map_err(err)?)),
            34 => Ok(Self::StatusChange(protocol::TmStatusChange::decode(data).map_err(err)?)),
            35 => Ok(Self::HaveTransactionSet(protocol::TmHaveTransactionSet::decode(data).map_err(err)?)),
            41 => Ok(Self::Validation(protocol::TmValidation::decode(data).map_err(err)?)),
            42 => Ok(Self::GetObjects(protocol::TmGetObjectByHash::decode(data).map_err(err)?)),
            54 => Ok(Self::ValidatorList(protocol::TmValidatorList::decode(data).map_err(err)?)),
            55 => Ok(Self::Squelch(protocol::TmSquelch::decode(data).map_err(err)?)),
            56 => Ok(Self::ValidatorListCollection(protocol::TmValidatorListCollection::decode(data).map_err(err)?)),
            57 => Ok(Self::ProofPathRequest(protocol::TmProofPathRequest::decode(data).map_err(err)?)),
            58 => Ok(Self::ProofPathResponse(protocol::TmProofPathResponse::decode(data).map_err(err)?)),
            59 => Ok(Self::ReplayDeltaRequest(protocol::TmReplayDeltaRequest::decode(data).map_err(err)?)),
            60 => Ok(Self::ReplayDeltaResponse(protocol::TmReplayDeltaResponse::decode(data).map_err(err)?)),
            63 => Ok(Self::HaveTransactions(protocol::TmHaveTransactions::decode(data).map_err(err)?)),
            64 => Ok(Self::Transactions(protocol::TmTransactions::decode(data).map_err(err)?)),
            _ => Err(NodeError::MessageDecode(format!("unknown type code: {type_code}"))),
        }
    }

    /// Human-readable name for logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Manifests(_) => "Manifests",
            Self::Ping(_) => "Ping",
            Self::Cluster(_) => "Cluster",
            Self::Endpoints(_) => "Endpoints",
            Self::Transaction(_) => "Transaction",
            Self::GetLedger(_) => "GetLedger",
            Self::LedgerData(_) => "LedgerData",
            Self::ProposeSet(_) => "ProposeSet",
            Self::StatusChange(_) => "StatusChange",
            Self::HaveTransactionSet(_) => "HaveTransactionSet",
            Self::Validation(_) => "Validation",
            Self::GetObjects(_) => "GetObjects",
            Self::ValidatorList(_) => "ValidatorList",
            Self::Squelch(_) => "Squelch",
            Self::ValidatorListCollection(_) => "ValidatorListCollection",
            Self::ProofPathRequest(_) => "ProofPathRequest",
            Self::ProofPathResponse(_) => "ProofPathResponse",
            Self::ReplayDeltaRequest(_) => "ReplayDeltaRequest",
            Self::ReplayDeltaResponse(_) => "ReplayDeltaResponse",
            Self::HaveTransactions(_) => "HaveTransactions",
            Self::Transactions(_) => "Transactions",
        }
    }
}
