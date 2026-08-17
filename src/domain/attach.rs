use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainAttachRequest {
    pub request_id: String,
    pub host: String,
    pub pane_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainBindOk {
    pub request_id: String,
    pub host: String,
    pub remote_socket: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainBindFail {
    pub request_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainSlotState {
    pub slot_id: u64,
    pub state: String,
    pub generation: u64,
}
