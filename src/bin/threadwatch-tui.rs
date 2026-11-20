use std::{
    io::{self, Stdout},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, Utc};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Tabs, Wrap,
    },
};
use serde::Deserialize;

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const PAGE_SIZE: usize = 100;

#[derive(Parser)]
#[command(version, about = "Read-only terminal viewer for a Threadwatch server")]
struct Cli {
    /// Threadwatch base URL. TW_URL is used when this option is omitted.
    #[arg(long, env = "TW_URL", default_value = "http://127.0.0.1:8787")]
    url: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadSummary {
    id: String,
    bot_name: String,
    #[serde(default)]
    short_title: String,
    #[serde(default)]
    title: String,
    status: String,
    #[serde(default)]
    reported_status: Option<String>,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    history_version: Option<String>,
    source: String,
    #[serde(default)]
    todo_count: usize,
    #[serde(default)]
    todo_completed: usize,
    #[serde(default)]
    event_count: usize,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    id: String,
    name: String,
    status: String,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    pending_batches: usize,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IndexResponse {
    threads: Vec<ThreadSummary>,
    sources: Vec<Source>,
}

#[derive(Clone, Debug, Deserialize)]
struct Todo {
    text: String,
    status: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    id: String,
    timestamp: i64,
    kind: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DetailResponse {
    thread: ThreadSummary,
    todos: Vec<Todo>,
    events: Vec<Record>,
    next_cursor: Option<String>,
    #[serde(default)]
    event_count: usize,
}

enum Request {
    Index,
    Detail { id: String, before: Option<String> },
    Stop,
}

enum Response {
    Index(Result<IndexResponse, String>),
    Detail {
        id: String,
        before: bool,
        result: Result<DetailResponse, String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    Active,
    Past,
}

impl Filter {
    fn matches(self, status: &str) -> bool {
        match self {
            Self::Active => matches!(status, "working" | "queued" | "waiting"),
            Self::Past => !matches!(status, "working" | "queued" | "waiting"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Threads,
    Detail,
}

struct App {
    base_url: String,
    threads: Vec<ThreadSummary>,
    sources: Vec<Source>,
    filtered: Vec<usize>,
    selected: usize,
    selected_id: Option<String>,
    detail: Option<DetailResponse>,
    filter: Filter,
    pane: Pane,
    show_todos: bool,
    show_activity: bool,
    paused: bool,
    search_mode: bool,
    search: String,
    message_selected: usize,
    expanded_message: Option<String>,
    scroll: u16,
    follow: bool,
    index_pending: bool,
    detail_pending: bool,
    older_pending: bool,
    last_success: Option<Instant>,
    error: Option<String>,
    should_quit: bool,
}

impl App {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            threads: Vec::new(),
            sources: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            selected_id: None,
            detail: None,
            filter: Filter::Active,
            pane: Pane::Threads,
            show_todos: false,
            show_activity: false,
            paused: false,
            search_mode: false,
            search: String::new(),
            message_selected: 0,
            expanded_message: None,
            scroll: u16::MAX,
            follow: true,
            index_pending: false,
            detail_pending: false,
            older_pending: false,
            last_success: None,
            error: None,
            should_quit: false,
        }
    }

    fn rebuild_filter(&mut self) {
        let selected_id = self.selected_id.as_deref();
        self.filtered = self
            .threads
            .iter()
            .enumerate()
            .filter_map(|(index, thread)| self.filter.matches(&thread.status).then_some(index))
            .collect();
        self.selected = selected_id
            .and_then(|id| self.filtered.iter().position(|&i| self.threads[i].id == id))
            .unwrap_or(0)
            .min(self.filtered.len().saturating_sub(1));
    }

    fn current_thread(&self) -> Option<&ThreadSummary> {
        self.filtered
            .get(self.selected)
            .map(|&index| &self.threads[index])
    }

    fn select_current(&mut self, requests: &Sender<Request>) {
        let Some(id) = self.current_thread().map(|thread| thread.id.clone()) else {
            self.selected_id = None;
            self.detail = None;
            return;
        };
        if self.selected_id.as_deref() == Some(&id) && self.detail.is_some() {
            return;
        }
        self.selected_id = Some(id.clone());
        self.detail = None;
        self.scroll = u16::MAX;
        self.follow = true;
        self.detail_pending = true;
        let _ = requests.send(Request::Detail { id, before: None });
    }

    fn refresh_detail(&mut self, requests: &Sender<Request>) {
        if self.paused || self.detail_pending {
            return;
        }
        let Some(id) = self.selected_id.clone() else {
            return;
        };
        self.detail_pending = true;
        let _ = requests.send(Request::Detail { id, before: None });
    }

    fn load_older(&mut self, requests: &Sender<Request>) {
        if self.paused || self.older_pending {
            return;
        }
        let Some(detail) = &self.detail else { return };
        let (Some(cursor), Some(id)) = (detail.next_cursor.clone(), self.selected_id.clone())
        else {
            return;
        };
        self.older_pending = true;
        self.follow = false;
        let _ = requests.send(Request::Detail {
            id,
            before: Some(cursor),
        });
    }

    fn visible_record_count(&self) -> usize {
        self.detail
            .as_ref()
            .map(|detail| {
                filtered_records(detail, self.show_activity, &self.search).count()
            })
            .unwrap_or(0)
    }

    fn move_message_selection(&mut self, delta: isize) {
        let count = self.visible_record_count();
        self.message_selected = move_message_index(self.message_selected, count, delta);
        self.follow = count > 0 && self.message_selected + 1 == count;
    }

    fn open_selected_message(&mut self) {
        self.expanded_message = self.detail.as_ref().and_then(|detail| {
            filtered_records(detail, self.show_activity, &self.search)
                .nth(self.message_selected)
                .map(|record| record.id.clone())
        });
        self.scroll = 0;
    }

    fn move_selection(&mut self, delta: isize, requests: &Sender<Request>) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.filtered.len() - 1);
        self.select_current(requests);
    }

    fn handle_key(&mut self, key: KeyEvent, requests: &Sender<Request>) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if self.search_mode {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.search_mode = false,
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Char(c) => self.search.push(c),
                _ => {}
            }
            return;
        }
        if self.expanded_message.is_some() {
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                    self.should_quit = true
                }
                (KeyCode::Esc | KeyCode::Enter, _) => {
                    self.expanded_message = None;
                    self.scroll = 0;
                }
                (KeyCode::Down | KeyCode::Char('j'), _) => {
                    self.scroll = self.scroll.saturating_add(1)
                }
                (KeyCode::Up | KeyCode::Char('k'), _) => {
                    self.scroll = self.scroll.saturating_sub(1)
                }
                (KeyCode::PageDown, _) => self.scroll = self.scroll.saturating_add(10),
                (KeyCode::PageUp, _) => self.scroll = self.scroll.saturating_sub(10),
                (KeyCode::Home, _) => self.scroll = 0,
                _ => {}
            }
            return;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                self.should_quit = true
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                self.pane = if self.pane == Pane::Threads {
                    Pane::Detail
                } else {
                    Pane::Threads
                };
            }
            (KeyCode::Char('a'), _) => {
                self.filter = Filter::Active;
                self.rebuild_filter();
                self.select_current(requests);
            }
            (KeyCode::Char('x'), _) => {
                self.filter = Filter::Past;
                self.rebuild_filter();
                self.select_current(requests);
            }
            (KeyCode::Char('/'), _) => {
                self.search_mode = true;
                self.search.clear();
            }
            (KeyCode::Char('p'), _) => {
                self.paused = !self.paused;
                if !self.paused {
                    self.refresh_detail(requests);
                }
            }
            (KeyCode::Char('f'), _) => {
                self.follow = true;
                self.message_selected = self.visible_record_count().saturating_sub(1);
            }
            (KeyCode::Char('t'), _) => self.show_todos = !self.show_todos,
            (KeyCode::Char('v'), _) => {
                self.show_activity = !self.show_activity;
                self.follow = true;
                self.message_selected = self.visible_record_count().saturating_sub(1);
            }
            (KeyCode::Enter, _) if self.pane == Pane::Detail => self.open_selected_message(),
            (KeyCode::Char('o'), _) => self.load_older(requests),
            (KeyCode::Char('r'), _) => {
                if !self.index_pending {
                    self.index_pending = true;
                    let _ = requests.send(Request::Index);
                }
                self.refresh_detail(requests);
            }
            (KeyCode::Down | KeyCode::Char('j'), _) if self.pane == Pane::Threads => {
                self.move_selection(1, requests)
            }
            (KeyCode::Up | KeyCode::Char('k'), _) if self.pane == Pane::Threads => {
                self.move_selection(-1, requests)
            }
            (KeyCode::Down | KeyCode::Char('j'), _) => self.move_message_selection(1),
            (KeyCode::Up | KeyCode::Char('k'), _) => self.move_message_selection(-1),
            (KeyCode::PageDown, _) => self.move_message_selection(5),
            (KeyCode::PageUp, _) => self.move_message_selection(-5),
            (KeyCode::Home, _) => {
                self.follow = false;
                self.message_selected = 0;
            }
            (KeyCode::End, _) => {
                self.follow = true;
                self.message_selected = self.visible_record_count().saturating_sub(1);
            }
            _ => {}
        }
    }

    fn apply(&mut self, response: Response, requests: &Sender<Request>) {
        match response {
            Response::Index(result) => {
                self.index_pending = false;
                match result {
                    Ok(index) => {
                        self.threads = index.threads;
                        self.sources = index.sources;
                        self.rebuild_filter();
                        self.error = None;
                        self.last_success = Some(Instant::now());
                        self.select_current(requests);
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            Response::Detail { id, before, result } => {
                if before {
                    self.older_pending = false;
                } else {
                    self.detail_pending = false;
                }
                if self.selected_id.as_deref() != Some(&id) {
                    return;
                }
                        let selected_record = self.detail.as_ref().and_then(|detail| {
                            filtered_records(detail, self.show_activity, &self.search)
                                .nth(self.message_selected)
                                .map(|record| record.id.clone())
                        });
                match result {
                    Ok(mut incoming) => {
                        if before {
                            if let Some(current) = &mut self.detail {
                                incoming.events.append(&mut current.events);
                            }
                        } else if let Some(current) = &mut self.detail
                            && current.thread.history_version == incoming.thread.history_version
                            && let Some(first) = incoming.events.first()
                            && let Some(overlap) =
                                current.events.iter().position(|event| event.id == first.id)
                        {
                            let mut loaded = current.events.drain(..overlap).collect::<Vec<_>>();
                            loaded.append(&mut incoming.events);
                            incoming.events = loaded;
                            incoming.next_cursor = current.next_cursor.clone();
                        }
                        self.detail = Some(incoming);
                        let count = self.visible_record_count();
                        self.message_selected = if self.follow {
                            count.saturating_sub(1)
                        } else {
                            selected_record
                                .as_deref()
                                .and_then(|id| {
                                    self.detail.as_ref().and_then(|detail| {
                                        filtered_records(detail, self.show_activity, &self.search)
                                            .position(|record| record.id == id)
                                    })
                                })
                                .unwrap_or_else(|| self.message_selected.min(count.saturating_sub(1)))
                        };
                        self.expanded_message = self.expanded_message.take().filter(|id| {
                            self.detail.as_ref().is_some_and(|detail| {
                                detail.events.iter().any(|record| record.id == *id)
                            })
                        });
                        self.error = None;
                        self.last_success = Some(Instant::now());
                    }
                    Err(error) => self.error = Some(error),
                }
            }
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("Threadwatch TUI: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let base_url = normalize_url(&cli.url)?;
    let (request_tx, request_rx) = mpsc::channel();
    let (response_tx, response_rx) = mpsc::channel();
    let worker_url = base_url.clone();
    let worker = thread::spawn(move || worker(worker_url, request_rx, response_tx));

    let mut app = App::new(base_url);
    app.index_pending = true;
    request_tx.send(Request::Index)?;
    let mut terminal = open_terminal()?;
    let result = event_loop(&mut terminal, &mut app, &request_tx, &response_rx);
    restore_terminal(&mut terminal)?;
    let _ = request_tx.send(Request::Stop);
    let _ = worker.join();
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    requests: &Sender<Request>,
    responses: &Receiver<Response>,
) -> Result<()> {
    let mut next_poll = Instant::now() + POLL_INTERVAL;
    while !app.should_quit {
        while let Ok(response) = responses.try_recv() {
            app.apply(response, requests);
        }
        terminal.draw(|frame| draw(frame, app))?;
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(100));
        if event::poll(wait)? {
            if let Event::Key(key) = event::read()? {
                app.handle_key(key, requests);
            }
        }
        if Instant::now() >= next_poll {
            if !app.index_pending {
                app.index_pending = true;
                let _ = requests.send(Request::Index);
            }
            app.refresh_detail(requests);
            next_poll = Instant::now() + POLL_INTERVAL;
        }
    }
    Ok(())
}

fn worker(base_url: String, requests: Receiver<Request>, responses: Sender<Response>) {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .build()
        .new_agent();
    while let Ok(request) = requests.recv() {
        match request {
            Request::Index => {
                let result = get_json(&agent, &format!("{base_url}/api/threads"));
                if responses.send(Response::Index(result)).is_err() {
                    break;
                }
            }
            Request::Detail { id, before } => {
                let mut url = format!("{base_url}/api/threads/{id}?limit={PAGE_SIZE}");
                if let Some(cursor) = &before {
                    url.push_str("&before=");
                    url.push_str(&urlencoding::encode(cursor));
                }
                let result = get_json(&agent, &url);
                if responses
                    .send(Response::Detail {
                        id,
                        before: before.is_some(),
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Request::Stop => break,
        }
    }
}

fn get_json<T: for<'de> Deserialize<'de>>(agent: &ureq::Agent, url: &str) -> Result<T, String> {
    agent
        .get(url)
        .header("accept", "application/json")
        .call()
        .map_err(|error| format!("request failed: {error}"))?
        .body_mut()
        .read_json()
        .map_err(|error| format!("invalid server response: {error}"))
}

fn normalize_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("URL must begin with http:// or https://");
    }
    if url.len() <= "https://".len() {
        bail!("URL must include a host");
    }
    Ok(url.to_owned())
}

fn open_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("could not enable terminal raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("could not enter alternate screen")?;
    Terminal::new(CrosstermBackend::new(stdout)).context("could not initialize terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("could not disable terminal raw mode")?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)
        .context("could not leave alternate screen")?;
    terminal.show_cursor().context("could not restore cursor")
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(frame.area());
    draw_header(frame, app, areas[0]);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(areas[1]);
    draw_threads(frame, app, columns[0]);
    draw_detail(frame, app, columns[1]);
    draw_footer(frame, app, areas[2]);
}

fn draw_header(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let active = app
        .threads
        .iter()
        .filter(|thread| Filter::Active.matches(&thread.status))
        .count();
    let tabs = Tabs::new(vec![
        format!("Active {active}"),
        format!("Past {}", app.threads.len().saturating_sub(active)),
    ])
    .select(usize::from(app.filter == Filter::Past))
    .highlight_style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
    .block(
        Block::default()
            .title(format!(" Threadwatch TUI · {} ", app.base_url))
            .borders(Borders::ALL),
    );
    frame.render_widget(tabs, area);
}

fn draw_threads(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&index| {
            let thread = &app.threads[index];
            let title = if thread.short_title.is_empty() {
                &thread.title
            } else {
                &thread.short_title
            };
            let state = if let Some(reported) = &thread.reported_status {
                format!("{} ({reported})", thread.status)
            } else {
                thread.status.clone()
            };
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        &thread.bot_name,
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" · "),
                    Span::raw(title),
                ]),
                Line::from(vec![
                    Span::styled(state, status_style(&thread.status)),
                    Span::raw(format!(
                        "  ✓{}/{}  {} events  {}",
                        thread.todo_completed,
                        thread.todo_count,
                        thread.event_count,
                        age(thread.updated_at)
                    )),
                ]),
            ])
        })
        .collect();
    let title = format!(" Threads · {} ", app.filtered.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(focus_style(app.pane == Pane::Threads));
    let list = List::new(items)
        .block(block)
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().bg(Color::DarkGray));
    let mut state =
        ListState::default().with_selected((!app.filtered.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let Some(detail) = &app.detail else {
        let message = if app.detail_pending {
            "Loading thread…"
        } else {
            "Select a thread."
        };
        frame.render_widget(
            Paragraph::new(message).block(
                Block::default()
                    .title(" Detail ")
                    .borders(Borders::ALL)
                    .border_style(focus_style(app.pane == Pane::Detail)),
            ),
            area,
        );
        return;
    };
    let task_height = if app.show_todos {
        (detail.todos.len() as u16)
            .saturating_add(3)
            .min(area.height / 2)
    } else {
        3
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(task_height),
            Constraint::Min(4),
        ])
        .split(area);
    let source = app
        .sources
        .iter()
        .find(|source| source.id == detail.thread.source);
    let source_text = source
        .map(|source| {
            let mut text = format!(
                "{}: {} · {} queued · {}",
                source.name,
                source.status,
                source.pending_batches,
                source
                    .last_seen_at
                    .map(age)
                    .unwrap_or_else(|| "never seen".into())
            );
            if let Some(error) = &source.error {
                text.push_str(" · ");
                text.push_str(error);
            }
            text
        })
        .unwrap_or_else(|| "Source unavailable".into());
    let header = Text::from(vec![
        Line::from(vec![
            Span::styled(
                &detail.thread.bot_name,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::raw(&detail.thread.title),
        ]),
        Line::from(vec![
            Span::styled(&detail.thread.status, status_style(&detail.thread.status)),
            Span::raw(format!(" · {source_text}")),
        ]),
    ]);
    frame.render_widget(
        Paragraph::new(header).wrap(Wrap { trim: true }).block(
            Block::default()
                .title(" Thread ")
                .borders(Borders::ALL)
                .border_style(focus_style(app.pane == Pane::Detail)),
        ),
        rows[0],
    );
    draw_todos(frame, app, detail, rows[1]);
    draw_transcript(frame, app, detail, rows[2]);
}

fn draw_todos(frame: &mut ratatui::Frame, app: &App, detail: &DetailResponse, area: Rect) {
    let completed = detail
        .todos
        .iter()
        .filter(|todo| todo.status == "completed")
        .count();
    let title = format!(" Tasks {completed}/{} · t toggles ", detail.todos.len());
    let lines = if !app.show_todos {
        vec![Line::raw(if detail.todos.is_empty() {
            "No task list recorded."
        } else {
            "Task list collapsed."
        })]
    } else if detail.todos.is_empty() {
        vec![Line::raw("No task list recorded.")]
    } else {
        detail
            .todos
            .iter()
            .map(|todo| {
                let mark = match todo.status.as_str() {
                    "completed" => "✓",
                    "in_progress" => "▶",
                    _ => "○",
                };
                Line::from(vec![
                    Span::styled(format!("{mark} "), status_style(&todo.status)),
                    Span::raw(&todo.text),
                ])
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().title(title).borders(Borders::ALL)),
        area,
    );
}

fn draw_transcript(frame: &mut ratatui::Frame, app: &App, detail: &DetailResponse, area: Rect) {
    let records: Vec<&Record> =
        filtered_records(detail, app.show_activity, &app.search).collect();
    if let Some(id) = &app.expanded_message
        && let Some(record) = detail.events.iter().find(|record| record.id == *id)
    {
        draw_full_message(frame, app, record, area);
        return;
    }
    let visible_total = detail
        .events
        .iter()
        .filter(|record| transcript_record_visible(record, app.show_activity))
        .count();
    let width = area.width.saturating_sub(6).max(12) as usize;
    let items = records
        .iter()
        .map(|record| {
            let (label, style) = if record.kind == "message" {
                let (label, style) = message_presentation(record);
                (label.to_owned(), style)
            } else {
                (
                    record_label(record),
                    if record.is_error {
                        Style::default().fg(Color::Red)
                    } else {
                        record_style(&record.kind)
                    },
                )
            };
            let (preview, truncated) = message_preview(&record.text, width, 4);
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    format!(" {label} "),
                    style.add_modifier(Modifier::REVERSED | Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", timestamp(record.timestamp)),
                    Style::default().fg(Color::DarkGray),
                ),
            ])];
            lines.extend(preview.into_iter().map(|line| Line::raw(format!("  {line}"))));
            if truncated {
                lines.push(Line::styled(
                    "  … Enter to read full message",
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::raw(""));
            ListItem::new(lines)
        })
        .collect::<Vec<_>>();
    let state = if app.paused {
        "paused"
    } else if app.follow {
        "following"
    } else {
        "held"
    };
    let older = if detail.next_cursor.is_some() {
        " · o older"
    } else {
        " · beginning loaded"
    };
    let view = if app.show_activity { "Activity" } else { "Messages" };
    let position = if records.is_empty() {
        0
    } else {
        app.message_selected.min(records.len() - 1) + 1
    };
    let title = format!(
        " {view} {position}/{} · {}/{} visible · {}/{} records · {state}{older} ",
        records.len(),
        records.len(),
        visible_total,
        detail.events.len(),
        detail.event_count.max(detail.events.len()),
    );
    if items.is_empty() {
        let message = if app.search.is_empty() {
            if app.show_activity {
                "Nothing recorded yet."
            } else {
                "No messages in loaded history. Press v to inspect all activity."
            }
        } else {
            "No matches in the current transcript view."
        };
        frame.render_widget(
            Paragraph::new(message)
                .block(Block::default().title(title).borders(Borders::ALL)),
            area,
        );
        return;
    }
    let list = List::new(items)
        .block(Block::default().title(title).borders(Borders::ALL))
        .highlight_symbol("▶ ")
        .highlight_style(Style::default().bg(Color::DarkGray));
    let mut list_state =
        ListState::default().with_selected(Some(app.message_selected.min(records.len() - 1)));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn draw_full_message(
    frame: &mut ratatui::Frame,
    app: &App,
    record: &Record,
    area: Rect,
) {
    let (label, style) = if record.kind == "message" {
        let (label, style) = message_presentation(record);
        (label.to_owned(), style)
    } else {
        (record_label(record), record_style(&record.kind))
    };
    let title = format!(
        " {label} · {} · Full message · Enter/Esc closes ",
        timestamp(record.timestamp)
    );
    let text = if record.text.is_empty() {
        "No text recorded."
    } else {
        &record.text
    };
    let paragraph = Paragraph::new(text)
        .scroll((app.scroll, 0))
        .wrap(Wrap { trim: false })
        .style(style)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(style),
        );
    frame.render_widget(paragraph, area);
    let width = area.width.saturating_sub(2).max(1) as usize;
    let content_height = text
        .lines()
        .map(|line| line.chars().count().max(1).div_ceil(width))
        .sum::<usize>();
    let mut scrollbar = ScrollbarState::new(content_height).position(app.scroll as usize);
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut scrollbar,
    );
}

fn filtered_records<'a>(
    detail: &'a DetailResponse,
    show_activity: bool,
    search: &str,
) -> impl Iterator<Item = &'a Record> {
    let needle = search.to_lowercase();
    detail
        .events
        .iter()
        .filter(move |record| transcript_record_visible(record, show_activity))
        .filter(move |record| {
            needle.is_empty()
                || record.text.to_lowercase().contains(&needle)
                || record_label(record).to_lowercase().contains(&needle)
        })
}

fn message_preview(text: &str, width: usize, max_lines: usize) -> (Vec<String>, bool) {
    let width = width.max(1);
    let mut output = Vec::with_capacity(max_lines);
    let mut truncated = false;
    for source in text.lines().chain(text.is_empty().then_some("")) {
        let chars = source.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            if output.len() == max_lines {
                truncated = true;
                break;
            }
            output.push(String::new());
            continue;
        }
        for chunk in chars.chunks(width) {
            if output.len() == max_lines {
                truncated = true;
                break;
            }
            output.push(chunk.iter().collect());
        }
        if truncated {
            break;
        }
    }
    (output, truncated)
}

fn move_message_index(current: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        0
    } else {
        current.saturating_add_signed(delta).min(count - 1)
    }
}

fn transcript_record_visible(record: &Record, show_activity: bool) -> bool {
    show_activity || record.kind == "message"
}

fn message_presentation(record: &Record) -> (&str, Style) {
    match record.role.as_deref() {
        Some("user") => ("YOU", Style::default().fg(Color::Cyan)),
        Some("assistant") => ("AGENT", Style::default().fg(Color::Green)),
        Some(role) => (role, Style::default().fg(Color::Magenta)),
        None => ("MESSAGE", Style::default().fg(Color::Blue)),
    }
}

fn draw_footer(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let left = if app.search_mode {
        format!("Search loaded history: {}_", app.search)
    } else {
        "Tab pane  j/k message  Enter read  / search  v activity  t tasks  p pause  f latest  o older  q quit".into()
    };
    let right = if let Some(error) = &app.error {
        Span::styled(
            format!(" · disconnected: {error}"),
            Style::default().fg(Color::Red),
        )
    } else {
        let age = app
            .last_success
            .map(|instant| format!("{}s ago", instant.elapsed().as_secs()))
            .unwrap_or_else(|| "connecting".into());
        Span::styled(
            format!(" · connected {age}"),
            Style::default().fg(Color::Green),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(left), right])),
        area,
    );
}

fn status_style(status: &str) -> Style {
    let color = match status {
        "working" | "in_progress" | "ready" => Color::Green,
        "queued" | "waiting" | "pending" => Color::Yellow,
        "failed" | "offline" | "unavailable" => Color::Red,
        "completed" => Color::Blue,
        _ => Color::Gray,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn record_style(kind: &str) -> Style {
    match kind {
        "message" => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        "tool_call" | "tool_start" => Style::default().fg(Color::Yellow),
        "tool_result" => Style::default().fg(Color::Green),
        "import_error" => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        _ => Style::default().fg(Color::Magenta),
    }
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn record_label(record: &Record) -> String {
    record
        .name
        .clone()
        .or_else(|| record.role.clone())
        .unwrap_or_else(|| record.kind.replace('_', " "))
}

fn timestamp(value: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(value)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".into())
}

fn age(value: i64) -> String {
    let Some(time) = DateTime::<Utc>::from_timestamp_millis(value) else {
        return "unknown".into();
    };
    let seconds = Utc::now().signed_duration_since(time).num_seconds().max(0);
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_url() {
        assert_eq!(
            normalize_url(" http://host:8787/// ").unwrap(),
            "http://host:8787"
        );
        assert!(normalize_url("host:8787").is_err());
    }

    #[test]
    fn active_filter_has_exact_server_states() {
        for status in ["working", "queued", "waiting"] {
            assert!(Filter::Active.matches(status));
            assert!(!Filter::Past.matches(status));
        }
        for status in ["completed", "failed", "stopped", "unknown"] {
            assert!(!Filter::Active.matches(status));
            assert!(Filter::Past.matches(status));
        }
    }


    #[test]
    fn message_navigation_clamps_to_loaded_records() {
        assert_eq!(move_message_index(0, 0, 1), 0);
        assert_eq!(move_message_index(0, 3, -1), 0);
        assert_eq!(move_message_index(1, 3, 1), 2);
        assert_eq!(move_message_index(2, 3, 1), 2);
    }

    #[test]
    fn message_preview_bounds_long_content() {
        let (preview, truncated) = message_preview("abcdefghij\nsecond\nthird", 4, 3);
        assert_eq!(preview, ["abcd", "efgh", "ij"]);
        assert!(truncated);
    }

    fn record(kind: &str, role: Option<&str>, text: &str) -> Record {
        Record {
            id: format!("{kind}-{text}"),
            timestamp: 0,
            kind: kind.into(),
            role: role.map(str::to_owned),
            name: None,
            text: text.into(),
            is_error: false,
        }
    }

    #[test]
    fn transcript_defaults_to_messages_and_can_reveal_activity() {
        let message = record("message", Some("assistant"), "done");
        let tool = record("tool_result", None, "raw output");
        assert!(transcript_record_visible(&message, false));
        assert!(!transcript_record_visible(&tool, false));
        assert!(transcript_record_visible(&message, true));
        assert!(transcript_record_visible(&tool, true));
    }

    #[test]
    fn message_roles_have_readable_labels() {
        assert_eq!(message_presentation(&record("message", Some("user"), "hi")).0, "YOU");
        assert_eq!(
            message_presentation(&record("message", Some("assistant"), "hello")).0,
            "AGENT"
        );
    }
}
