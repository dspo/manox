//! Full-screen chat TUI for Cx Agent.

use std::cmp::{max, min};
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use tui_textarea::TextArea;

use crate::cx_agent::approval::{ApprovalDecision, ApprovalMode};
use crate::cx_agent::events::CxStreamEvent;
use crate::cx_agent::rollout::TokenUsageRecord;
use crate::cx_agent::tui::approval_prompt::{
    ApprovalPromptMetrics, ApprovalPromptState, ApprovalRequest, render_approval_preview_lines,
};

const HISTORY_SCROLL_STEP: usize = 3;
const INPUT_MAX_HEIGHT: usize = 8;
const BANNER_TEXT: &str =
    "Ready. Enter sends, Alt-Enter adds a newline, PgUp/PgDn scrolls, /quit exits.";

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

#[derive(Debug, Clone)]
pub enum ChatEvent {
    Input(String),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Continue,
    Aborted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum UiMode {
    Chat,
    Streaming,
    Approval,
}

impl UiMode {
    fn label(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Streaming => "streaming",
            Self::Approval => "approval",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageRole {
    User,
    Assistant,
    System,
    Error,
}

impl MessageRole {
    fn prefix(self) -> &'static str {
        match self {
            Self::User => "you> ",
            Self::Assistant => "cx > ",
            Self::System => "sys> ",
            Self::Error => "err> ",
        }
    }

    fn style(self) -> Style {
        match self {
            Self::User => Style::default().fg(Color::Cyan),
            Self::Assistant => Style::default().fg(Color::Green),
            Self::System => Style::default().fg(Color::Yellow),
            Self::Error => Style::default().fg(Color::Red),
        }
    }
}

#[derive(Debug, Clone)]
struct ChatMessage {
    role: MessageRole,
    text: String,
    meta: Option<String>,
}

impl ChatMessage {
    fn new(role: MessageRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            meta: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct HistoryMetrics {
    total_lines: usize,
    viewport_lines: usize,
}

impl HistoryMetrics {
    fn max_scroll(self) -> usize {
        self.total_lines.saturating_sub(self.viewport_lines.max(1))
    }

    fn page_step(self) -> usize {
        self.viewport_lines
            .saturating_sub(1)
            .max(HISTORY_SCROLL_STEP)
    }
}

#[derive(Debug, Clone)]
struct RenderSnapshot {
    provider: String,
    model: String,
    wire_api: String,
    approval_mode: ApprovalMode,
    session_id: String,
    messages: Vec<ChatMessage>,
    input: TextArea<'static>,
    ui_mode: UiMode,
    approval_prompt: Option<ApprovalPromptState>,
    history_scroll: usize,
    cumulative_usage: TokenUsageRecord,
    status_note: Option<String>,
}

struct TerminalSession {
    terminal: AppTerminal,
    restored: bool,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )
        .context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("failed to initialize terminal")?;
        terminal.hide_cursor().ok();
        terminal.clear().ok();
        Ok(Self {
            terminal,
            restored: false,
        })
    }

    fn restore(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }
        disable_raw_mode().ok();
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableBracketedPaste,
            DisableMouseCapture
        )
        .ok();
        self.terminal.show_cursor().ok();
        self.restored = true;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub struct ChatApp {
    provider: String,
    model: String,
    wire_api: String,
    approval_mode: ApprovalMode,
    session_id: String,
    rollout_path: PathBuf,
    terminal: Option<TerminalSession>,
    input: TextArea<'static>,
    messages: Vec<ChatMessage>,
    ui_mode: UiMode,
    active_assistant: Option<usize>,
    showed_reasoning_prefix: bool,
    history_scroll: usize,
    follow_output: bool,
    history_metrics: HistoryMetrics,
    cumulative_usage: TokenUsageRecord,
    status_note: Option<String>,
    pending_quit: bool,
    fatal_error: Option<String>,
}

impl ChatApp {
    pub fn new(
        provider: String,
        model: String,
        wire_api: String,
        approval_mode: ApprovalMode,
        session_id: String,
        rollout_path: PathBuf,
    ) -> Self {
        let mut app = Self {
            provider,
            model,
            wire_api,
            approval_mode,
            session_id,
            rollout_path,
            terminal: None,
            input: build_input_box(),
            messages: vec![ChatMessage::new(MessageRole::System, BANNER_TEXT)],
            ui_mode: UiMode::Chat,
            active_assistant: None,
            showed_reasoning_prefix: false,
            history_scroll: 0,
            follow_output: true,
            history_metrics: HistoryMetrics::default(),
            cumulative_usage: TokenUsageRecord::default(),
            status_note: None,
            pending_quit: false,
            fatal_error: None,
        };
        app.push_system(format!(
            "Provider: {}  Model: {}  API: {}  Approval: {}",
            app.provider,
            app.model,
            app.wire_api,
            approval_mode_label(app.approval_mode)
        ));
        app
    }

    pub async fn read_user_input(&mut self) -> Result<ChatEvent> {
        self.check_fatal()?;
        if self.pending_quit {
            return Ok(ChatEvent::Quit);
        }
        self.ui_mode = UiMode::Chat;
        self.draw()?;

        loop {
            self.check_fatal()?;
            let event = event::read().context("failed to read terminal event")?;
            match event {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    if is_quit_key(key) {
                        self.pending_quit = true;
                        return Ok(ChatEvent::Quit);
                    }
                    if self.handle_history_key(key) {
                        self.draw()?;
                        continue;
                    }
                    if is_submit_key(key) {
                        let text = self.take_input_text();
                        let trimmed = text.trim();
                        if trimmed.is_empty() {
                            self.draw()?;
                            continue;
                        }
                        match trimmed {
                            "/quit" | "/exit" | ":q" => {
                                self.pending_quit = true;
                                return Ok(ChatEvent::Quit);
                            }
                            "/help" => {
                                self.push_system(
                                    "Commands: /quit /exit :q /help /usage".to_string(),
                                );
                                self.draw()?;
                                continue;
                            }
                            "/usage" => {
                                self.push_system(self.usage_summary_line());
                                self.draw()?;
                                continue;
                            }
                            _ => {
                                self.push_user(text.clone());
                                self.follow_output = true;
                                self.draw()?;
                                return Ok(ChatEvent::Input(text));
                            }
                        }
                    }
                    if is_newline_key(key) {
                        self.input.insert_str("\n");
                        self.draw()?;
                        continue;
                    }
                    if is_ctrl_d_quit(key) && input_is_blank(&self.input) {
                        self.pending_quit = true;
                        return Ok(ChatEvent::Quit);
                    }
                    self.input.input(key);
                    self.draw()?;
                }
                Event::Mouse(mouse) if self.handle_mouse(mouse) => {
                    self.draw()?;
                }
                Event::Mouse(_) => {}
                Event::Paste(text) => {
                    self.input.insert_str(text);
                    self.draw()?;
                }
                Event::Resize(_, _) => {
                    self.draw()?;
                }
                _ => {}
            }
        }
    }

    pub fn begin_assistant(&mut self) {
        self.ui_mode = UiMode::Streaming;
        self.status_note = Some("Assistant is responding...".to_string());
        self.showed_reasoning_prefix = false;
        self.active_assistant = Some(self.messages.len());
        self.messages
            .push(ChatMessage::new(MessageRole::Assistant, String::new()));
        self.follow_output = true;
        if let Err(err) = self.draw() {
            self.fatal_error = Some(format!("failed to draw streaming view: {err}"));
        }
    }

    pub fn end_assistant(&mut self, elapsed: Duration, usage: TokenUsageRecord) {
        self.ui_mode = UiMode::Chat;
        self.active_assistant = None;
        self.showed_reasoning_prefix = false;
        self.cumulative_usage.input += usage.input;
        self.cumulative_usage.output += usage.output;
        self.cumulative_usage.cache_read += usage.cache_read;
        self.cumulative_usage.cache_write += usage.cache_write;
        self.cumulative_usage.reasoning += usage.reasoning;
        self.status_note = Some(format!(
            "Last turn: {:.1}s  in={} out={} cache_r={} cache_w={} reasoning={}",
            elapsed.as_secs_f32(),
            usage.input,
            usage.output,
            usage.cache_read,
            usage.cache_write,
            usage.reasoning
        ));
        if let Some(last) = self.messages.last_mut() {
            if last.role == MessageRole::Assistant && last.text.is_empty() {
                last.text = "(empty response)".to_string();
            }
            if last.role == MessageRole::Assistant {
                last.meta = Some(format!(
                    "[{:.1}s | in={} out={} cache_r={} cache_w={} reasoning={}]",
                    elapsed.as_secs_f32(),
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    usage.cache_write,
                    usage.reasoning
                ));
            }
        }
        if let Err(err) = self.draw() {
            self.fatal_error = Some(format!("failed to redraw after response: {err}"));
        }
    }

    pub fn show_error(&mut self, msg: &str) {
        self.push_error(msg.to_string());
        self.status_note = Some(format!("Error: {msg}"));
        if let Err(err) = self.draw() {
            self.fatal_error = Some(format!("failed to redraw error state: {err}"));
            eprintln!("Error: {msg}");
        }
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.restore_terminal()?;
        println!("Cx Agent session saved to {}", self.rollout_path.display());
        Ok(())
    }

    pub async fn handle_stream_event(
        &mut self,
        event: &Result<CxStreamEvent>,
    ) -> Result<RunOutcome> {
        self.check_fatal()?;
        if let Some(outcome) = self.process_background_events()? {
            return Ok(outcome);
        }

        let outcome = match event {
            Ok(event) => self.apply_stream_event(event),
            Err(err) => {
                let msg = err.to_string();
                self.push_error(format!("Stream error: {msg}"));
                self.status_note = Some(format!("Stream error: {msg}"));
                RunOutcome::Aborted
            }
        };
        self.draw()?;
        Ok(outcome)
    }

    #[allow(dead_code)]
    pub fn prompt_approval(&mut self, request: ApprovalRequest) -> Result<ApprovalDecision> {
        self.check_fatal()?;
        self.ensure_terminal()?;

        let previous_mode = self.ui_mode;
        self.ui_mode = UiMode::Approval;
        let mut prompt = ApprovalPromptState::new(request);
        let decision = loop {
            if let Some(metrics) = self.draw_with_prompt(Some(&prompt))? {
                prompt.scroll = min(prompt.scroll, metrics.max_scroll());
            }
            let event = event::read().context("failed to read approval prompt event")?;
            match event {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if matches!(
                        key.code,
                        KeyCode::Char('a') | KeyCode::Char('y') | KeyCode::Enter
                    ) {
                        break ApprovalDecision::Allow;
                    }
                    if matches!(
                        key.code,
                        KeyCode::Char('d') | KeyCode::Char('n') | KeyCode::Esc
                    ) {
                        break ApprovalDecision::Deny {
                            reason: "denied in TUI".to_string(),
                        };
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'))
                    {
                        self.pending_quit = true;
                        break ApprovalDecision::Deny {
                            reason: "cancelled with Ctrl-C".to_string(),
                        };
                    }
                    if prompt.handle_key(key) {
                        continue;
                    }
                }
                Event::Mouse(mouse) if prompt.handle_mouse(mouse) => continue,
                Event::Mouse(_) => {}
                Event::Resize(_, _) => continue,
                _ => {}
            }
        };

        self.ui_mode = previous_mode;
        self.draw()?;
        Ok(decision)
    }

    fn ensure_terminal(&mut self) -> Result<()> {
        if self.terminal.is_none() {
            self.terminal = Some(TerminalSession::enter()?);
        }
        Ok(())
    }

    fn restore_terminal(&mut self) -> Result<()> {
        if let Some(mut session) = self.terminal.take() {
            session.restore()?;
        }
        Ok(())
    }

    fn draw(&mut self) -> Result<()> {
        let _ = self.draw_with_prompt(None)?;
        Ok(())
    }

    fn draw_with_prompt(
        &mut self,
        prompt: Option<&ApprovalPromptState>,
    ) -> Result<Option<ApprovalPromptMetrics>> {
        self.ensure_terminal()?;
        let snapshot = self.snapshot(prompt.cloned());
        let terminal = self.terminal.as_mut().expect("terminal initialized");
        let mut history_metrics = self.history_metrics;
        let mut approval_metrics: Option<ApprovalPromptMetrics> = None;
        terminal
            .terminal
            .draw(|frame| {
                let (metrics, prompt_metrics) = draw_ui(frame, &snapshot);
                history_metrics = metrics;
                approval_metrics = prompt_metrics;
            })
            .context("failed to draw chat UI")?;
        self.history_metrics = history_metrics;
        if self.follow_output {
            self.history_scroll = self.history_metrics.max_scroll();
        } else {
            self.history_scroll = min(self.history_scroll, self.history_metrics.max_scroll());
        }
        Ok(approval_metrics)
    }

    fn snapshot(&self, prompt: Option<ApprovalPromptState>) -> RenderSnapshot {
        RenderSnapshot {
            provider: self.provider.clone(),
            model: self.model.clone(),
            wire_api: self.wire_api.clone(),
            approval_mode: self.approval_mode,
            session_id: self.session_id.clone(),
            messages: self.messages.clone(),
            input: self.input.clone(),
            ui_mode: self.ui_mode,
            approval_prompt: prompt,
            history_scroll: self.history_scroll,
            cumulative_usage: self.cumulative_usage,
            status_note: self.status_note.clone(),
        }
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> bool {
        let page = self.history_metrics.page_step();
        match (key.code, key.modifiers) {
            (KeyCode::PageUp, _) => {
                self.scroll_history(-(page as isize));
                true
            }
            (KeyCode::PageDown, _) => {
                self.scroll_history(page as isize);
                true
            }
            (KeyCode::Up, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_history(-1);
                true
            }
            (KeyCode::Down, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.scroll_history(1);
                true
            }
            (KeyCode::Home, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.history_scroll = 0;
                self.follow_output = false;
                true
            }
            (KeyCode::End, mods) if mods.contains(KeyModifiers::CONTROL) => {
                self.follow_output = true;
                self.history_scroll = self.history_metrics.max_scroll();
                true
            }
            _ => false,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_history(-(HISTORY_SCROLL_STEP as isize));
                true
            }
            MouseEventKind::ScrollDown => {
                self.scroll_history(HISTORY_SCROLL_STEP as isize);
                true
            }
            _ => false,
        }
    }

    fn process_background_events(&mut self) -> Result<Option<RunOutcome>> {
        let mut redraw = false;
        while event::poll(Duration::from_millis(0)).context("failed to poll terminal event")? {
            match event::read().context("failed to read terminal event")? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if is_quit_key(key) {
                        self.pending_quit = true;
                        self.status_note =
                            Some("Stopping after current stream chunk...".to_string());
                        return Ok(Some(RunOutcome::Aborted));
                    }
                    if self.handle_history_key(key) {
                        redraw = true;
                    }
                }
                Event::Mouse(mouse) if self.handle_mouse(mouse) => redraw = true,
                Event::Mouse(_) => {}
                Event::Resize(_, _) => redraw = true,
                _ => {}
            }
        }
        if redraw {
            self.draw()?;
        }
        Ok(None)
    }

    fn apply_stream_event(&mut self, event: &CxStreamEvent) -> RunOutcome {
        match event {
            CxStreamEvent::TextDelta(text) => {
                self.append_assistant_text(text);
                if self.showed_reasoning_prefix {
                    self.status_note = Some("Assistant is responding...".to_string());
                    self.showed_reasoning_prefix = false;
                }
            }
            CxStreamEvent::ReasoningDelta(_) => {
                if !self.showed_reasoning_prefix {
                    self.status_note = Some("Assistant is reasoning...".to_string());
                    self.showed_reasoning_prefix = true;
                }
            }
            CxStreamEvent::ToolCallStart { id, name } => {
                self.push_system(format!("Tool call started: {name} ({id})"));
            }
            CxStreamEvent::ToolCallArgsDelta { id, partial } => {
                self.status_note = Some(format!(
                    "Tool {id} arguments streaming... ({} chars)",
                    partial.len()
                ));
            }
            CxStreamEvent::ToolCallDone { name, .. } => {
                self.push_system(format!("Tool call finished: {name}"));
            }
            CxStreamEvent::Usage { .. } => {}
            CxStreamEvent::Done => {
                self.status_note = Some("Assistant response completed.".to_string());
            }
            CxStreamEvent::Error(msg) => {
                self.push_error(format!("Stream error: {msg}"));
                self.status_note = Some(format!("Stream error: {msg}"));
                return RunOutcome::Aborted;
            }
        }
        RunOutcome::Continue
    }

    fn append_assistant_text(&mut self, delta: &str) {
        let idx = self.active_assistant.unwrap_or_else(|| {
            self.messages
                .push(ChatMessage::new(MessageRole::Assistant, String::new()));
            let idx = self.messages.len() - 1;
            self.active_assistant = Some(idx);
            idx
        });
        if let Some(message) = self.messages.get_mut(idx) {
            message.text.push_str(delta);
        }
    }

    fn push_user(&mut self, text: String) {
        self.messages
            .push(ChatMessage::new(MessageRole::User, text));
    }

    fn push_system(&mut self, text: String) {
        self.messages
            .push(ChatMessage::new(MessageRole::System, text));
    }

    fn push_error(&mut self, text: String) {
        self.messages
            .push(ChatMessage::new(MessageRole::Error, text));
        self.follow_output = true;
    }

    fn take_input_text(&mut self) -> String {
        let text = self.input.lines().join("\n");
        self.input = build_input_box();
        text
    }

    fn usage_summary_line(&self) -> String {
        format!(
            "Usage: in={} out={} cache_r={} cache_w={} reasoning={}",
            self.cumulative_usage.input,
            self.cumulative_usage.output,
            self.cumulative_usage.cache_read,
            self.cumulative_usage.cache_write,
            self.cumulative_usage.reasoning
        )
    }

    fn scroll_history(&mut self, delta: isize) {
        let max_scroll = self.history_metrics.max_scroll() as isize;
        let next = (self.history_scroll as isize + delta).clamp(0, max_scroll);
        self.history_scroll = next as usize;
        self.follow_output = self.history_scroll >= self.history_metrics.max_scroll();
    }

    fn check_fatal(&mut self) -> Result<()> {
        if let Some(message) = self.fatal_error.take() {
            return Err(anyhow!(message));
        }
        Ok(())
    }
}

impl Drop for ChatApp {
    fn drop(&mut self) {
        let _ = self.restore_terminal();
    }
}

fn draw_ui(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &RenderSnapshot,
) -> (HistoryMetrics, Option<ApprovalPromptMetrics>) {
    let area = frame.area();
    let input_height = compute_input_height(&snapshot.input);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),
            Constraint::Length(input_height as u16),
            Constraint::Length(2),
        ])
        .split(area);

    let history_metrics = render_history(frame, layout[0], snapshot);
    render_input(frame, layout[1], &snapshot.input);
    render_status(frame, layout[2], snapshot);

    let prompt_metrics = snapshot
        .approval_prompt
        .as_ref()
        .map(|prompt| render_approval_prompt(frame, area, prompt));
    (history_metrics, prompt_metrics)
}

fn render_history(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    snapshot: &RenderSnapshot,
) -> HistoryMetrics {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Conversation ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return HistoryMetrics::default();
    }

    let (content_area, scrollbar_area) = split_scrollbar_area(inner);
    let content_width = content_area.width.max(1) as usize;
    let rendered = render_history_lines(&snapshot.messages, content_width);
    let total_lines = max(rendered.len(), 1);
    let viewport_lines = content_area.height as usize;
    let max_scroll = total_lines.saturating_sub(viewport_lines.max(1));
    let scroll = if snapshot.history_scroll >= max_scroll {
        max_scroll
    } else {
        snapshot.history_scroll
    };

    let paragraph = Paragraph::new(rendered)
        .scroll((scroll.min(u16::MAX as usize) as u16, 0))
        .style(Style::default());
    frame.render_widget(paragraph, content_area);

    if scrollbar_area.width > 0 && scrollbar_area.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let mut state = ScrollbarState::new(total_lines)
            .position(scroll)
            .viewport_content_length(viewport_lines.max(1));
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    }

    HistoryMetrics {
        total_lines,
        viewport_lines,
    }
}

fn render_input(frame: &mut ratatui::Frame<'_>, area: Rect, input: &TextArea<'static>) {
    frame.render_widget(input, area);
}

fn render_status(frame: &mut ratatui::Frame<'_>, area: Rect, snapshot: &RenderSnapshot) {
    let line1 = format!(
        "provider:{}  model:{}  api:{}  session:{}  approval:{}  mode:{}  usage:{}/{}/r{}/c{}/w{}",
        snapshot.provider,
        snapshot.model,
        snapshot.wire_api,
        snapshot.session_id,
        approval_mode_label(snapshot.approval_mode),
        snapshot.ui_mode.label(),
        snapshot.cumulative_usage.input,
        snapshot.cumulative_usage.output,
        snapshot.cumulative_usage.reasoning,
        snapshot.cumulative_usage.cache_read,
        snapshot.cumulative_usage.cache_write,
    );
    let line2 = snapshot.status_note.clone().unwrap_or_else(|| match snapshot.ui_mode {
        UiMode::Chat => {
            "Enter send | Alt-Enter newline | Ctrl-Home/End jump | PgUp/PgDn scroll | Esc/Ctrl-C quit"
                .to_string()
        }
        UiMode::Streaming => {
            "Streaming... PgUp/PgDn scroll history | Esc/Ctrl-C stop and exit".to_string()
        }
        UiMode::Approval => {
            "Approval prompt open".to_string()
        }
    });

    let paragraph = Paragraph::new(vec![
        Line::from(Span::styled(
            line1,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(line2, Style::default().fg(Color::DarkGray))),
    ]);
    frame.render_widget(paragraph, area);
}

fn render_approval_prompt(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    prompt: &ApprovalPromptState,
) -> ApprovalPromptMetrics {
    let modal = centered_rect(area, 80, 70);
    frame.render_widget(Clear, modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Approval required ");
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    if inner.width == 0 || inner.height == 0 {
        return ApprovalPromptMetrics::default();
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);

    let header = Paragraph::new(vec![
        Line::from(format!("tool: {}", prompt.request.tool_name)),
        Line::from(format!("category: {}", prompt.request.category_label())),
        Line::from("review the arguments preview before allowing the tool call"),
    ])
    .style(Style::default().fg(Color::White));
    frame.render_widget(header, sections[0]);

    let preview_block = Block::default().borders(Borders::ALL).title(" Arguments ");
    let preview_inner = preview_block.inner(sections[1]);
    frame.render_widget(preview_block, sections[1]);
    let (preview_content, preview_scrollbar) = split_scrollbar_area(preview_inner);
    let preview_width = preview_content.width.max(1) as usize;
    let rendered = render_approval_preview_lines(&prompt.request.arguments_preview, preview_width);
    let total_lines = rendered.len().max(1);
    let viewport_lines = preview_content.height as usize;
    let max_scroll = total_lines.saturating_sub(viewport_lines.max(1));
    let scroll = min(prompt.scroll, max_scroll);
    let preview = Paragraph::new(rendered).scroll((scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(preview, preview_content);
    if preview_scrollbar.width > 0 && preview_scrollbar.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let mut state = ScrollbarState::new(total_lines)
            .position(scroll)
            .viewport_content_length(viewport_lines.max(1));
        frame.render_stateful_widget(scrollbar, preview_scrollbar, &mut state);
    }

    let footer = Paragraph::new(vec![Line::from(
        "[Enter/a] allow  [d/n/Esc] deny  [PgUp/PgDn] scroll  [Ctrl-C] deny + quit",
    )])
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, sections[2]);

    ApprovalPromptMetrics {
        total_lines,
        viewport_lines,
    }
}

fn render_history_lines(messages: &[ChatMessage], width: usize) -> Vec<Line<'static>> {
    let safe_width = width.max(1);
    let mut lines = Vec::new();
    for (idx, message) in messages.iter().enumerate() {
        for text in wrap_with_prefix(message.role.prefix(), &message.text, safe_width) {
            lines.push(Line::from(Span::styled(text, message.role.style())));
        }
        if let Some(meta) = &message.meta {
            for text in wrap_with_prefix("     ", meta, safe_width) {
                lines.push(Line::from(Span::styled(
                    text,
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        if idx + 1 != messages.len() {
            lines.push(Line::from(""));
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("sys> No messages yet."));
    }
    lines
}

fn split_scrollbar_area(area: Rect) -> (Rect, Rect) {
    if area.width <= 1 {
        return (area, Rect::default());
    }
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    (chunks[0], chunks[1])
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn build_input_box() -> TextArea<'static> {
    let mut input = TextArea::default();
    input.set_block(Block::default().borders(Borders::ALL).title(" Input "));
    input.set_placeholder_text("Type a prompt. Enter sends. Alt-Enter inserts a newline.");
    input.set_style(Style::default().fg(Color::White));
    input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    input.set_cursor_line_style(Style::default());
    input
}

fn compute_input_height(input: &TextArea<'static>) -> usize {
    let lines = input.lines().len().max(1);
    min(lines + 2, INPUT_MAX_HEIGHT)
}

fn approval_mode_label(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::AlwaysAllow => "always-allow",
        ApprovalMode::PerCall => "per-call",
        ApprovalMode::ReadOnlyAutoAllow => "read-only-auto-allow",
    }
}

fn input_is_blank(input: &TextArea<'static>) -> bool {
    input.lines().iter().all(|line| line.trim().is_empty())
}

fn is_quit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc)
        || (key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')))
}

fn is_ctrl_d_quit(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('d'))
}

fn is_submit_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
}

fn is_newline_key(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Enter)
        && (key.modifiers.contains(KeyModifiers::ALT)
            || key.modifiers.contains(KeyModifiers::SHIFT))
}

fn wrap_with_prefix(prefix: &str, text: &str, width: usize) -> Vec<String> {
    let safe_width = width.max(1);
    let continuation = " ".repeat(prefix.len());
    let available = safe_width.saturating_sub(prefix.len()).max(1);
    let mut lines = Vec::new();

    if text.is_empty() {
        lines.push(prefix.to_string());
        return lines;
    }

    for raw in text.split('\n') {
        if raw.is_empty() {
            lines.push(prefix.to_string());
            continue;
        }
        let mut rest = raw;
        let mut first = true;
        while !rest.is_empty() {
            let take = split_at_char_boundary(rest, available);
            let (head, tail) = rest.split_at(take);
            let label = if first { prefix } else { &continuation };
            lines.push(format!("{label}{head}"));
            rest = tail;
            first = false;
        }
    }

    lines
}

fn split_at_char_boundary(text: &str, max_chars: usize) -> usize {
    if text.chars().count() <= max_chars {
        return text.len();
    }
    let mut count = 0;
    for (idx, ch) in text.char_indices() {
        if count == max_chars {
            return idx;
        }
        count += 1;
        if idx + ch.len_utf8() == text.len() && count <= max_chars {
            return text.len();
        }
    }
    text.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_ascii_lines_with_prefix() {
        let lines = wrap_with_prefix("you> ", "abcdefgh", 8);
        assert_eq!(lines, vec!["you> abc", "     def", "     gh"]);
    }

    #[test]
    fn preserves_blank_lines_when_wrapping() {
        let lines = wrap_with_prefix("sys> ", "hello\n\nworld", 20);
        assert_eq!(lines, vec!["sys> hello", "sys> ", "sys> world"]);
    }

    #[test]
    fn history_metrics_compute_max_scroll() {
        let metrics = HistoryMetrics {
            total_lines: 20,
            viewport_lines: 6,
        };
        assert_eq!(metrics.max_scroll(), 14);
        assert_eq!(metrics.page_step(), 5);
    }
}
