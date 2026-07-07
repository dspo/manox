//! End-to-end smoke for `manox --mcp`: spawn the binary, connect an rmcp
//! client over stdio, drive the Harness through MCP tool calls.
//!
//! Both tests are `#[ignore]`-gated: `manox --mcp` opens a real gpui window,
//! which needs a window server (macOS local dev works; headless CI does not).
//! `mcp_smoke_send_message` additionally needs the live provider config at
//! `~/.config/cx/` and `MANOX_RUN_LIVE=1`.
//!
//! Run: `cargo test -p manox --features debug --test mcp_smoke -- --ignored --nocapture`

#![cfg(feature = "debug")]

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::TokioChildProcess;

/// The 13 v1 tools the server must advertize. Kept in sync with
/// `mcp_server::tool_list`.
const EXPECTED_TOOLS: &[&str] = &[
    "manox_new_thread",
    "manox_open_thread",
    "manox_list_threads",
    "manox_send_message",
    "manox_send_command",
    "manox_approve",
    "manox_plan_respond",
    "manox_cancel",
    "manox_read_conversation",
    "manox_read_messages",
    "manox_is_running",
    "manox_await_idle",
    "manox_quit",
];

/// Locate the `manox` binary built by `cargo test`. `CARGO_BIN_EXE_manox` is
/// set by cargo for integration tests (Rust 1.43+).
fn manox_bin() -> String {
    std::env::var("CARGO_BIN_EXE_manox").expect("CARGO_BIN_EXE_manox set by cargo")
}

/// Spawn `manox --mcp` and connect an rmcp client to its stdio.
async fn connect() -> Result<rmcp::service::RunningService<rmcp::service::RoleClient, ()>> {
    let mut cmd = tokio::process::Command::new(manox_bin());
    cmd.arg("--mcp");
    let transport = TokioChildProcess::new(cmd)?;
    let client = ().serve(transport).await?;
    Ok(client)
}

#[tokio::test]
#[ignore = "opens a real gpui window; needs a window server"]
async fn mcp_smoke_tool_list() -> Result<()> {
    let client = connect().await?;

    let tools = client.list_all_tools().await?;
    let names: HashSet<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in EXPECTED_TOOLS {
        assert!(
            names.contains(*expected),
            "missing MCP tool `{expected}` (got: {:?})",
            names
        );
    }
    assert_eq!(
        names.len(),
        EXPECTED_TOOLS.len(),
        "unexpected extra tools advertised: {:?}",
        names
    );

    // `manox_read_conversation` with no turn run should return an empty item
    // list — proves the round-trip dispatches to the dispatcher and back.
    let res = client
        .peer()
        .call_tool(CallToolRequestParams::new("manox_read_conversation"))
        .await?;
    assert!(
        res.is_error != Some(true),
        "manox_read_conversation returned an error: {:?}",
        res.content
    );

    let _ = client
        .peer()
        .call_tool(CallToolRequestParams::new("manox_quit"))
        .await;
    client.cancel().await?;
    Ok(())
}

/// Live: send a real message, await idle, read conversation. Needs the
/// provider config and `MANOX_RUN_LIVE=1`.
#[tokio::test]
#[ignore = "live provider + MANOX_RUN_LIVE required; opens a real gpui window"]
async fn mcp_smoke_send_message() -> Result<()> {
    if std::env::var("MANOX_RUN_LIVE").is_err() {
        return Ok(()); // early-exit, not a failure
    }
    let client = connect().await?;

    let res = client
        .peer()
        .call_tool(
            CallToolRequestParams::new("manox_send_message").with_arguments(
                serde_json::Map::from_iter([("text".into(), "Say hi in one word.".into())]),
            ),
        )
        .await?;
    assert!(
        res.is_error != Some(true),
        "manox_send_message returned an error: {:?}",
        res.content
    );

    let _ = tokio::time::timeout(
        Duration::from_secs(90),
        client.peer().call_tool(
            CallToolRequestParams::new("manox_await_idle").with_arguments(
                serde_json::Map::from_iter([("timeout_ms".into(), 90_000.into())]),
            ),
        ),
    )
    .await
    .map_err(|_| anyhow::anyhow!("manox_await_idle timed out"))??;

    let conv = client
        .peer()
        .call_tool(CallToolRequestParams::new("manox_read_conversation"))
        .await?;
    assert!(conv.is_error != Some(true));
    // The tool result text is the serialized `{ items: [...] }` JSON.
    let text = conv
        .content
        .first()
        .and_then(|c| c.as_text().map(|t| t.text.clone()))
        .unwrap_or_default();
    assert!(
        !text.is_empty(),
        "manox_read_conversation returned empty content"
    );

    let _ = client
        .peer()
        .call_tool(CallToolRequestParams::new("manox_quit"))
        .await;
    client.cancel().await?;
    Ok(())
}
