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
            "{}\t{}\t{}",
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
