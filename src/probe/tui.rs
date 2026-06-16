use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event;
use crossterm::event::{Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::probe::db;
use crate::probe::types::{ProbeCellResult, ProbeRow, ProbeStatus};
use crate::{CxConfig, WireApi, CopilotAuth};

pub struct ProbeApp {
    pub rows: Vec<ProbeRow>,
    pub selected_row: usize,
    pub scroll_offset: usize,
    pub is_probing: bool,
    pub completed_count: usize,
    pub total_count: usize,
    pub spinner_tick: usize,
}

pub struct ProbeResultItem {
    pub row_idx: usize,
    pub wire_api: WireApi,
    pub result: Result<ProbeCellResult>,
}

pub fn run_tui(
    rows: Vec<ProbeRow>,
    config: &CxConfig,
    conn: &rusqlite::Connection,
) -> Result<()> {
    enable_raw_mode().context("启用 raw mode 失败")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("进入 alt screen 失败")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("初始化 terminal 失败")?;

    let mut app = ProbeApp {
        rows,
        selected_row: 0,
        scroll_offset: 0,
        is_probing: false,
        completed_count: 0,
        total_count: 0,
        spinner_tick: 0,
    };

    let result = event_loop(&mut terminal, &mut app, config, conn);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut ProbeApp,
    config: &CxConfig,
    conn: &rusqlite::Connection,
) -> Result<()> {
    let mut result_rx: Option<Receiver<ProbeResultItem>> = None;

    loop {
        terminal.draw(|f| super::view::draw(f, app))?;

        if event::poll(Duration::from_millis(50))? {
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char('r') if !app.is_probing => {
                    start_probing(app, config, &mut result_rx)?;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if app.selected_row < app.rows.len().saturating_sub(1) {
                        app.selected_row += 1;
                        ensure_visible(app, terminal.size()?.height as usize);
                    }
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if app.selected_row > 0 {
                        app.selected_row -= 1;
                        ensure_visible(app, terminal.size()?.height as usize);
                    }
                }
                _ => {}
            }
        }

        if app.is_probing {
            app.spinner_tick += 1;
            if let Some(ref rx) = result_rx {
                while let Ok(item) = rx.try_recv() {
                    if item.row_idx < app.rows.len() {
                        let row = &app.rows[item.row_idx];

                        // 获取当前的 configured 状态
                        let configured = row.results.get(&item.wire_api).map(|r| r.configured).unwrap_or(true);

                        let result = match item.result {
                            Ok(result) => ProbeCellResult {
                                configured,
                                ..result
                            },
                            Err(e) => {
                                ProbeCellResult {
                                    status: ProbeStatus::ClientError,
                                    latency_ms: None,
                                    http_status: None,
                                    error_message: Some(format!("探测异常: {e}")),
                                    configured,
                                }
                            }
                        };

                        db::save_probe_result(
                            conn,
                            &row.provider_name,
                            &row.model_id,
                            item.wire_api,
                            &result,
                        )?;

                        if let Some(cell) = app.rows[item.row_idx].results.get_mut(&item.wire_api)
                        {
                            *cell = result;
                        }
                    }
                    app.completed_count += 1;
                }

                if app.completed_count >= app.total_count {
                    app.is_probing = false;
                    result_rx = None;
                }
            }
        }
    }

    Ok(())
}

fn ensure_visible(app: &mut ProbeApp, visible_height: usize) {
    if app.rows.is_empty() {
        return;
    }

    // Header: 3, Footer: 1, Table borders: 2
    let table_height = visible_height.saturating_sub(6);

    if app.selected_row < app.scroll_offset {
        app.scroll_offset = app.selected_row;
    } else if app.selected_row >= app.scroll_offset + table_height {
        app.scroll_offset = app.selected_row.saturating_sub(table_height.saturating_sub(1));
    }
}

fn start_probing(
    app: &mut ProbeApp,
    config: &CxConfig,
    result_rx: &mut Option<Receiver<ProbeResultItem>>,
) -> Result<()> {
    let (tx, rx) = channel();

    let mut probe_items = Vec::new();
    let mut total = 0;

    for (row_idx, row) in app.rows.iter().enumerate() {
        for wire_api in [WireApi::Anthropic, WireApi::Responses, WireApi::Completions] {
            if let Some(provider) = config.providers.iter().find(|p| p.name == row.provider_name) {
                let endpoint = provider
                    .normalized_endpoints()
                    .into_iter()
                    .find(|e| WireApi::from_str(&e.wire_api) == wire_api);

                if let Some(endpoint) = endpoint {
                    let auth = CopilotAuth::from_endpoint(&endpoint);
                    probe_items.push((row_idx, wire_api, provider.clone(), endpoint.url, row.model_id.clone(), auth));
                    total += 1;
                }
            }
        }
    }

    for (row_idx, wire_api, _, _, _, _) in &probe_items {
        if let Some(cell) = app.rows[*row_idx].results.get_mut(wire_api) {
            cell.status = ProbeStatus::Probing;
        }
    }

    // 限制并发探测数量
    const MAX_CONCURRENT: usize = 10;
    let active_count = Arc::new((Mutex::new(0usize), Condvar::new()));

    for (row_idx, wire_api, provider, url, model_id, auth) in probe_items {
        let tx = tx.clone();
        let active_count = active_count.clone();

        std::thread::spawn(move || {
            // 等待有空位（限制并发）
            let (count, cv) = &*active_count;
            let mut count = count.lock().unwrap();
            while *count >= MAX_CONCURRENT {
                count = cv.wait(count).unwrap();
            }
            *count += 1;
            drop(count);

            let result = super::do_probe(&provider, &url, wire_api, &model_id, auth);
            let _ = tx.send(ProbeResultItem {
                row_idx,
                wire_api,
                result,
            });

            // 释放并发槽位
            let (count, cv) = &*active_count;
            let mut count = count.lock().unwrap();
            *count -= 1;
            cv.notify_one();
        });
    }

    app.is_probing = true;
    app.completed_count = 0;
    app.total_count = total;
    *result_rx = Some(rx);

    Ok(())
}
