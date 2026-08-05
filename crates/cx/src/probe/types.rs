use std::collections::HashMap;

use crate::WireApi;

#[derive(Debug, Clone)]
pub struct ProbeRow {
    pub provider_name: String,
    pub model_id: String,
    pub results: HashMap<WireApi, ProbeCellResult>,
}

#[derive(Debug, Clone)]
pub struct ProbeCellResult {
    pub status: ProbeStatus,
    pub latency_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub error_message: Option<String>,
    /// 用户是否配置了该 wire_api
    pub configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStatus {
    Available,
    NotApplicable,
    ServerError,
    ClientError,
    Probing,
    Unknown,
}
