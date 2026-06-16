use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params, Connection};

use super::types::{ProbeCellResult, ProbeStatus};
use crate::WireApi;

pub fn init_probe_schema(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS probe_results (
            provider_name TEXT NOT NULL,
            model_id      TEXT NOT NULL,
            wire_api      TEXT NOT NULL,
            status        TEXT NOT NULL,
            latency_ms    INTEGER,
            http_status   INTEGER,
            error_message TEXT,
            probed_at     TEXT NOT NULL,
            PRIMARY KEY (provider_name, model_id, wire_api)
        )",
        [],
    )?;
    Ok(())
}

pub fn load_probe_results(conn: &Connection) -> Result<HashMap<String, ProbeCellResult>> {
    let mut stmt = conn.prepare(
        "SELECT provider_name, model_id, wire_api, status, latency_ms, http_status, error_message
         FROM probe_results",
    )?;

    let rows = stmt.query_map([], |row| {
        let wire_api_str: String = row.get(2)?;
        let status_str: String = row.get(3)?;

        let status = match status_str.as_str() {
            "available" => ProbeStatus::Available,
            "not_applicable" => ProbeStatus::NotApplicable,
            "server_error" => ProbeStatus::ServerError,
            "client_error" => ProbeStatus::ClientError,
            "probing" => ProbeStatus::Probing,
            _ => ProbeStatus::Unknown,
        };

        let key = format!(
            "{}\0{}\0{}",
            row.get::<_, String>(0)?,
            wire_api_str,
            row.get::<_, String>(1)?
        );

        Ok((
            key,
            ProbeCellResult {
                status,
                latency_ms: row.get(4)?,
                http_status: row.get(5)?,
                error_message: row.get(6)?,
                configured: true,
            },
        ))
    })?;

    let mut results = HashMap::new();
    for row in rows {
        let (key, result) = row?;
        results.insert(key, result);
    }

    Ok(results)
}

/// 获取指定 provider+model 的最佳可用 wire_api
/// 返回状态为 Available 的 wire_api 中优先级最高的一个
pub fn get_available_wire_api(
    conn: &Connection,
    provider_name: &str,
    model_id: &str,
) -> Result<Option<WireApi>> {
    let mut stmt = conn.prepare(
        "SELECT wire_api FROM probe_results
         WHERE provider_name = ?1 AND model_id = ?2 AND status = 'available'",
    )?;

    let rows = stmt.query_map([provider_name, model_id], |row| {
        let wire_api_str: String = row.get(0)?;
        Ok(wire_api_str)
    })?;

    let mut best_api: Option<WireApi> = None;
    let mut best_priority = u8::MAX;

    for row in rows {
        let wire_api_str = row?;
        let wire_api = match wire_api_str.as_str() {
            "anthropic" => WireApi::Anthropic,
            "responses" => WireApi::Responses,
            "completions" => WireApi::Completions,
            _ => continue,
        };

        let priority = wire_api.priority();
        if priority < best_priority {
            best_priority = priority;
            best_api = Some(wire_api);
        }
    }

    Ok(best_api)
}

pub fn save_probe_result(
    conn: &Connection,
    provider_name: &str,
    model_id: &str,
    wire_api: WireApi,
    result: &ProbeCellResult,
) -> Result<()> {
    let status_str = match result.status {
        ProbeStatus::Available => "available",
        ProbeStatus::NotApplicable => "not_applicable",
        ProbeStatus::ServerError => "server_error",
        ProbeStatus::ClientError => "client_error",
        ProbeStatus::Probing => "probing",
        ProbeStatus::Unknown => "unknown",
    };

    conn.execute(
        "INSERT OR REPLACE INTO probe_results (provider_name, model_id, wire_api, status, latency_ms, http_status, error_message, probed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'))",
        params![
            provider_name,
            model_id,
            wire_api.display(),
            status_str,
            result.latency_ms,
            result.http_status,
            result.error_message,
        ],
    )?;

    Ok(())
}
