//! TUI 主循环与应用状态。

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;
use std::time::Duration;

use super::types::{Period, UsageRecord, View};
use super::view::draw;

pub(super) struct StatsApp {
    pub(super) records: Vec<UsageRecord>,
    pub(super) today: String,
    pub(super) view: View,
    pub(super) period: Period,
    pub(super) models_scroll: usize,
    pub(super) matrix_scroll: usize,
}

impl StatsApp {
    pub(super) fn new(records: Vec<UsageRecord>, today: String) -> Self {
        Self {
            records,
            today,
            view: View::Models,
            period: Period::Last7,
            models_scroll: 0,
            matrix_scroll: 0,
        }
    }

    /// 当前 period 内的记录。
    pub(super) fn period_records(&self) -> Vec<&UsageRecord> {
        self.records
            .iter()
            .filter(|r| self.period.includes(&r.date, &self.today))
            .collect()
    }
}

pub(super) fn run_tui(records: Vec<UsageRecord>, today: String) -> Result<()> {
    enable_raw_mode().context("启用 raw mode 失败")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("进入 alt screen 失败")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("初始化 terminal 失败")?;

    let result = event_loop(&mut terminal, records, today);

    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    records: Vec<UsageRecord>,
    today: String,
) -> Result<()> {
    let mut app = StatsApp::new(records, today);
    loop {
        terminal.draw(|f| draw(f, &mut app))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Tab | KeyCode::BackTab => {
                app.view = match app.view {
                    View::Models => View::Matrix,
                    View::Matrix => View::Models,
                };
            }
            KeyCode::Char('1') => app.period = Period::Last7,
            KeyCode::Char('2') => app.period = Period::Last30,
            KeyCode::Char('3') => app.period = Period::All,
            KeyCode::Char('r') => app.period = app.period.cycle(),
            KeyCode::Down | KeyCode::Char('j') => match app.view {
                View::Models => app.models_scroll = app.models_scroll.saturating_add(1),
                View::Matrix => app.matrix_scroll = app.matrix_scroll.saturating_add(1),
            },
            KeyCode::Up | KeyCode::Char('k') => match app.view {
                View::Models => app.models_scroll = app.models_scroll.saturating_sub(1),
                View::Matrix => app.matrix_scroll = app.matrix_scroll.saturating_sub(1),
            },
            _ => {}
        }
    }
    Ok(())
}
