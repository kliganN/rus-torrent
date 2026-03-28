use crate::{
    config::AppConfig,
    path_completion::{
        collect_candidates, resolve_user_path, CompletionCandidate, PathCompletionMode,
    },
    torrent::{format_bytes, TorrentEngine, TorrentSnapshot, TorrentSource},
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use std::time::Duration;

const TICK_RATE: Duration = Duration::from_millis(200);
const MAX_VISIBLE_COMPLETIONS: usize = 8;

pub struct App {
    config: AppConfig,
    engine: TorrentEngine,
    downloads: Vec<TorrentSnapshot>,
    menu_state: ListState,
    downloads_state: ListState,
    focus: FocusArea,
    form_field: FormField,
    torrent_source: String,
    download_dir: String,
    completion_state: Option<CompletionState>,
    status_line: String,
    should_quit: bool,
}

#[derive(Clone, Debug)]
struct CompletionState {
    field: FormField,
    seed_input: String,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

impl CompletionState {
    fn current_replacement(&self) -> &str {
        self.candidates[self.selected].replacement.as_str()
    }

    fn step(&mut self, direction: CompletionDirection) {
        self.selected = match direction {
            CompletionDirection::Forward => (self.selected + 1) % self.candidates.len(),
            CompletionDirection::Backward => {
                if self.selected == 0 {
                    self.candidates.len() - 1
                } else {
                    self.selected - 1
                }
            }
        };
    }

    fn can_continue(&self, field: FormField, current_input: &str) -> bool {
        self.field == field
            && !self.candidates.is_empty()
            && (current_input == self.seed_input || current_input == self.current_replacement())
    }
}

#[derive(Clone, Debug)]
struct CompletionPreview {
    candidates: Vec<CompletionCandidate>,
    selected: Option<usize>,
    start_index: usize,
    total_matches: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FocusArea {
    Menu,
    Content,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormField {
    TorrentSource,
    DownloadDir,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Screen {
    AddTorrent = 0,
    Downloads = 1,
    Server = 2,
}

impl App {
    pub fn new(config: AppConfig, engine: TorrentEngine) -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(Screen::AddTorrent.index()));

        Self {
            download_dir: config.default_download_dir.display().to_string(),
            status_line: format!(
                "Incoming folder reserved for a future Telegram bot: {}",
                config.incoming_torrents_dir.display()
            ),
            config,
            engine,
            downloads: Vec::new(),
            menu_state,
            downloads_state: ListState::default(),
            focus: FocusArea::Content,
            form_field: FormField::TorrentSource,
            torrent_source: String::new(),
            completion_state: None,
            should_quit: false,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            self.refresh_downloads();
            terminal.draw(|frame| self.render(frame))?;

            if event::poll(TICK_RATE)? {
                let event = event::read()?;
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.handle_key(key).await?;
                    }
                    Event::Paste(text) => self.handle_paste(&text),
                    _ => {}
                }
            }
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::F(10))
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c')))
        {
            self.should_quit = true;
            return Ok(());
        }

        match key.code {
            KeyCode::Left => {
                self.focus = FocusArea::Menu;
                return Ok(());
            }
            KeyCode::Right => {
                self.focus = FocusArea::Content;
                return Ok(());
            }
            KeyCode::Esc => {
                self.focus = FocusArea::Menu;
                return Ok(());
            }
            _ => {}
        }

        match self.focus {
            FocusArea::Menu => {
                self.handle_menu_key(key);
                Ok(())
            }
            FocusArea::Content => match self.current_screen() {
                Screen::AddTorrent => self.handle_add_torrent_key(key).await,
                Screen::Downloads => {
                    self.handle_downloads_key(key);
                    Ok(())
                }
                Screen::Server => Ok(()),
            },
        }
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.select_previous_screen(),
            KeyCode::Down => self.select_next_screen(),
            KeyCode::Enter => self.focus = FocusArea::Content,
            _ => {}
        }
    }

    async fn handle_add_torrent_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up => self.form_field = self.form_field.previous(),
            KeyCode::Down => self.form_field = self.form_field.next(),
            KeyCode::Tab => self.complete_active_input(CompletionDirection::Forward)?,
            KeyCode::BackTab => self.complete_active_input(CompletionDirection::Backward)?,
            KeyCode::Backspace => self.pop_active_input(),
            KeyCode::Delete => self.clear_active_input(),
            KeyCode::Enter => self.submit_torrent().await?,
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.push_active_input(ch);
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_downloads_key(&mut self, key: KeyEvent) {
        if self.downloads.is_empty() {
            return;
        }

        let current = self.downloads_state.selected().unwrap_or(0);

        match key.code {
            KeyCode::Up => {
                let next = current.saturating_sub(1);
                self.downloads_state.select(Some(next));
            }
            KeyCode::Down => {
                let next = (current + 1).min(self.downloads.len().saturating_sub(1));
                self.downloads_state.select(Some(next));
            }
            _ => {}
        }
    }

    async fn submit_torrent(&mut self) -> Result<()> {
        let source_input = self
            .active_value(FormField::TorrentSource)
            .trim()
            .to_string();
        let download_dir = resolve_user_path(self.active_value(FormField::DownloadDir))
            .context("invalid output directory")?;

        match self
            .engine
            .add_torrent_source(&source_input, &download_dir)
            .await
        {
            Ok(id) => {
                self.status_line = format!(
                    "Torrent #{id} queued: {} -> {}",
                    source_input,
                    download_dir.display()
                );
                self.torrent_source.clear();
                self.clear_completion();
                self.set_screen(Screen::Downloads);
                self.focus = FocusArea::Content;
                self.refresh_downloads();
            }
            Err(error) => {
                self.status_line = format!("Failed to queue torrent: {error:#}");
            }
        }

        Ok(())
    }

    fn complete_active_input(&mut self, direction: CompletionDirection) -> Result<()> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim().to_string();

        if !field.supports_local_completion(&current_input) {
            self.clear_completion();
            self.status_line =
                "Local path completion is disabled for HTTP/HTTPS and magnet sources".to_string();
            return Ok(());
        }

        if let Some(state) = &mut self.completion_state {
            if state.can_continue(field, &current_input) {
                state.step(direction);
                let replacement = state.current_replacement().to_string();
                let total = state.candidates.len();
                let index = state.selected + 1;
                self.set_active_value(field, replacement);
                self.status_line =
                    format!("{} completion {index}/{total}", field.completion_subject());
                return Ok(());
            }
        }

        let completion_set = collect_candidates(self.active_value(field), field.completion_mode())?;

        if completion_set.candidates.is_empty() {
            self.clear_completion();
            self.status_line = format!("No {} matches found", field.completion_subject());
            return Ok(());
        }

        if completion_set.candidates.len() > 1 {
            if let Some(prefix) = completion_set.common_prefix() {
                self.clear_completion();
                self.set_active_value(field, prefix.clone());
                self.status_line = format!("Expanded to common prefix: {prefix}");
                return Ok(());
            }
        }

        let selected = match direction {
            CompletionDirection::Forward => 0,
            CompletionDirection::Backward => completion_set.candidates.len() - 1,
        };
        let replacement = completion_set.candidates[selected].replacement.clone();
        let total = completion_set.candidates.len();

        self.set_active_value(field, replacement);
        self.completion_state = Some(CompletionState {
            field,
            seed_input: completion_set.seed_input,
            candidates: completion_set.candidates,
            selected,
        });
        self.status_line = format!(
            "{} completion {}/{}",
            field.completion_subject(),
            selected + 1,
            total
        );

        Ok(())
    }

    fn refresh_downloads(&mut self) {
        self.downloads = self.engine.list_downloads();

        if self.downloads.is_empty() {
            self.downloads_state.select(None);
            return;
        }

        let selected = self.downloads_state.selected().unwrap_or(0);
        let clamped = selected.min(self.downloads.len() - 1);
        self.downloads_state.select(Some(clamped));
    }

    fn push_active_input(&mut self, character: char) {
        self.clear_completion();

        match self.form_field {
            FormField::TorrentSource => self.torrent_source.push(character),
            FormField::DownloadDir => self.download_dir.push(character),
        }
    }

    fn pop_active_input(&mut self) {
        self.clear_completion();

        match self.form_field {
            FormField::TorrentSource => {
                self.torrent_source.pop();
            }
            FormField::DownloadDir => {
                self.download_dir.pop();
            }
        }
    }

    fn clear_active_input(&mut self) {
        self.clear_completion();

        match self.form_field {
            FormField::TorrentSource => self.torrent_source.clear(),
            FormField::DownloadDir => self.download_dir.clear(),
        }
    }

    fn set_active_value(&mut self, field: FormField, value: String) {
        match field {
            FormField::TorrentSource => self.torrent_source = value,
            FormField::DownloadDir => self.download_dir = value,
        }
    }

    fn active_value(&self, field: FormField) -> &str {
        match field {
            FormField::TorrentSource => self.torrent_source.as_str(),
            FormField::DownloadDir => self.download_dir.as_str(),
        }
    }

    fn clear_completion(&mut self) {
        self.completion_state = None;
    }

    fn handle_paste(&mut self, text: &str) {
        if self.focus != FocusArea::Content || self.current_screen() != Screen::AddTorrent {
            return;
        }

        let sanitized = text.trim_matches(|character| character == '\n' || character == '\r');
        if sanitized.is_empty() {
            return;
        }

        self.clear_completion();

        match self.form_field {
            FormField::TorrentSource => self.torrent_source.push_str(sanitized),
            FormField::DownloadDir => self.download_dir.push_str(sanitized),
        }

        self.status_line = format!("Pasted into {}", self.form_field.title());
    }

    fn current_screen(&self) -> Screen {
        Screen::from_index(self.menu_state.selected().unwrap_or(0))
    }

    fn set_screen(&mut self, screen: Screen) {
        self.menu_state.select(Some(screen.index()));
    }

    fn select_previous_screen(&mut self) {
        let next = self.current_screen().index().saturating_sub(1);
        self.menu_state.select(Some(next));
    }

    fn select_next_screen(&mut self) {
        let current = self.current_screen().index();
        let next = (current + 1).min(Screen::all().len() - 1);
        self.menu_state.select(Some(next));
    }

    fn completion_preview(&self) -> Result<CompletionPreview> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim();

        if !field.supports_local_completion(current_input) {
            return Ok(CompletionPreview {
                candidates: Vec::new(),
                selected: None,
                start_index: 0,
                total_matches: 0,
            });
        }

        let (candidates, selected) = match &self.completion_state {
            Some(state) if state.can_continue(field, current_input) => {
                (state.candidates.clone(), Some(state.selected))
            }
            _ => {
                let completion =
                    collect_candidates(self.active_value(field), field.completion_mode())?;
                (completion.candidates, None)
            }
        };

        let total_matches = candidates.len();
        let (start_index, end_index) = completion_window(total_matches, selected);
        let visible_candidates = candidates[start_index..end_index].to_vec();
        let selected = selected.map(|index| index.saturating_sub(start_index));

        Ok(CompletionPreview {
            candidates: visible_candidates,
            selected,
            start_index,
            total_matches,
        })
    }

    fn completion_empty_text(&self) -> Text<'static> {
        if self.form_field == FormField::TorrentSource
            && !self
                .form_field
                .supports_local_completion(self.active_value(self.form_field))
        {
            return Text::from(vec![
                Line::from("This field currently contains a URL or magnet link."),
                Line::from("Press Enter to start downloading it immediately."),
                Line::from("Local path completion is only available for filesystem paths."),
            ]);
        }

        match self.form_field {
            FormField::TorrentSource => Text::from(vec![
                Line::from("This field accepts three source types:"),
                Line::from("1. Local .torrent files"),
                Line::from("2. HTTP/HTTPS URLs to .torrent files"),
                Line::from("3. magnet: links"),
                Line::from(""),
                Line::from("For local files, press Tab to autocomplete from /."),
            ]),
            FormField::DownloadDir => Text::from(vec![
                Line::from("Type a path and press Tab to autocomplete."),
                Line::from("An empty field starts browsing from /."),
                Line::from("Only directories are listed here."),
            ]),
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(4),
            ])
            .split(frame.area());

        self.render_header(frame, root[0]);

        let content = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(0)])
            .split(root[1]);

        self.render_menu(frame, content[0]);

        match self.current_screen() {
            Screen::AddTorrent => self.render_add_torrent(frame, content[1]),
            Screen::Downloads => self.render_downloads(frame, content[1]),
            Screen::Server => self.render_server(frame, content[1]),
        }

        self.render_footer(frame, root[2]);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let focus = match self.focus {
            FocusArea::Menu => "menu",
            FocusArea::Content => "content",
        };

        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                "rus-torrent",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                "Interactive torrent client",
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled(
                format!("focus: {focus}"),
                Style::default().fg(Color::Yellow),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL).title("Overview"));

        frame.render_widget(header, area);
    }

    fn render_menu(&mut self, frame: &mut Frame, area: Rect) {
        let items = Screen::all()
            .iter()
            .map(|screen| ListItem::new(Line::from(screen.title())))
            .collect::<Vec<_>>();

        let border_style = if self.focus == FocusArea::Menu {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let menu = List::new(items)
            .block(
                Block::default()
                    .title("Menu")
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(menu, area, &mut self.menu_state);
    }

    fn render_add_torrent(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Min(0),
            ])
            .split(area);

        self.render_input(
            frame,
            chunks[0],
            FormField::TorrentSource.title(),
            &self.torrent_source,
            self.focus == FocusArea::Content && self.form_field == FormField::TorrentSource,
            FormField::TorrentSource.placeholder(),
        );
        self.render_input(
            frame,
            chunks[1],
            FormField::DownloadDir.title(),
            &self.download_dir,
            self.focus == FocusArea::Content && self.form_field == FormField::DownloadDir,
            FormField::DownloadDir.placeholder(),
        );

        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(chunks[2]);

        self.render_completion_panel(frame, bottom[0]);
        self.render_add_help(frame, bottom[1]);
    }

    fn render_completion_panel(&self, frame: &mut Frame, area: Rect) {
        let preview = self.completion_preview();
        match preview {
            Ok(preview) if preview.candidates.is_empty() => {
                let empty = Paragraph::new(self.completion_empty_text())
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .title(format!("{} Completion", self.form_field.short_title()))
                            .borders(Borders::ALL),
                    );
                frame.render_widget(empty, area);
            }
            Ok(preview) => {
                let items = preview
                    .candidates
                    .iter()
                    .map(|candidate| {
                        ListItem::new(Line::from(vec![
                            Span::raw(candidate.replacement.as_str()),
                            Span::raw("  "),
                            Span::styled(
                                format!("[{}]", candidate.kind_label()),
                                Style::default().fg(if candidate.is_dir {
                                    Color::Green
                                } else {
                                    Color::Blue
                                }),
                            ),
                        ]))
                    })
                    .collect::<Vec<_>>();

                let mut list_state = ListState::default();
                list_state.select(preview.selected);

                let title = if preview.total_matches > preview.candidates.len() {
                    format!(
                        "{} Completion (showing {}-{} of {})",
                        self.form_field.short_title(),
                        preview.start_index + 1,
                        preview.start_index + preview.candidates.len(),
                        preview.total_matches
                    )
                } else {
                    format!(
                        "{} Completion ({})",
                        self.form_field.short_title(),
                        preview.total_matches
                    )
                };

                let list = List::new(items)
                    .block(Block::default().title(title).borders(Borders::ALL))
                    .highlight_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol(">> ");

                frame.render_stateful_widget(list, area, &mut list_state);
            }
            Err(error) => {
                let message = Paragraph::new(Text::from(vec![
                    Line::from("Autocomplete is currently unavailable."),
                    Line::from(format!("{error:#}")),
                ]))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .title("Path Completion")
                        .borders(Borders::ALL),
                );
                frame.render_widget(message, area);
            }
        }
    }

    fn render_add_help(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(8), Constraint::Min(0)])
            .split(area);

        let controls = Paragraph::new(Text::from(vec![
            Line::from(format!("Active field: {}", self.form_field.title())),
            Line::from("Tab: next completion match for local paths"),
            Line::from("Shift+Tab: previous completion match"),
            Line::from("Up/Down: switch field"),
            Line::from("Empty field + Tab: browse from /"),
            Line::from("Paste: local path, HTTP/HTTPS .torrent URL, or magnet link"),
            Line::from("Enter: add torrent"),
            Line::from("Esc: return to menu"),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Controls").borders(Borders::ALL));

        frame.render_widget(controls, chunks[0]);

        let future = Paragraph::new(Text::from(vec![
            Line::from(format!(
                "Incoming .torrent folder: {}",
                self.config.incoming_torrents_dir.display()
            )),
            Line::from("This directory is reserved for future Telegram bot uploads."),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Server Prep").borders(Borders::ALL));

        frame.render_widget(future, chunks[1]);
    }

    fn render_downloads(&mut self, frame: &mut Frame, area: Rect) {
        if self.downloads.is_empty() {
            let empty = Paragraph::new(Text::from(vec![
                Line::from("No torrents have been added yet."),
                Line::from("Open 'Choose torrent source' and queue a source."),
            ]))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Downloads").borders(Borders::ALL));
            frame.render_widget(empty, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
            .split(area);

        let items = self
            .downloads
            .iter()
            .map(|download| {
                ListItem::new(vec![
                    Line::from(download.name.as_str()),
                    Line::from(format!(
                        "#{}  {}  {:.1}%",
                        download.id,
                        download.state,
                        download.progress_ratio * 100.0
                    )),
                    Line::from(format!(
                        "↓ {}  peers {}",
                        download.download_speed.as_deref().unwrap_or("n/a"),
                        download.live_peers
                    )),
                ])
            })
            .collect::<Vec<_>>();

        let border_style = if self.focus == FocusArea::Content {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .title("Active Downloads")
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");

        frame.render_stateful_widget(list, chunks[0], &mut self.downloads_state);

        let selected = self
            .downloads_state
            .selected()
            .and_then(|index| self.downloads.get(index));

        if let Some(download) = selected {
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(13),
                    Constraint::Min(0),
                ])
                .split(chunks[1]);

            let label = if download.total_bytes == 0 {
                "Waiting for metadata".to_string()
            } else {
                format!(
                    "{} / {}",
                    format_bytes(download.progress_bytes),
                    format_bytes(download.total_bytes)
                )
            };

            let gauge = Gauge::default()
                .block(Block::default().title("Progress").borders(Borders::ALL))
                .gauge_style(Style::default().fg(Color::Cyan).bg(Color::Black))
                .ratio(download.progress_ratio)
                .label(label);

            frame.render_widget(gauge, right[0]);

            let stats = Paragraph::new(Text::from(vec![
                Line::from(format!("State: {}", download.state)),
                Line::from(format!(
                    "Finished: {}",
                    if download.finished { "yes" } else { "no" }
                )),
                Line::from(format!(
                    "Downloaded: {}",
                    format_bytes(download.progress_bytes)
                )),
                Line::from(format!(
                    "Uploaded: {}",
                    format_bytes(download.uploaded_bytes)
                )),
                Line::from(format!(
                    "Download speed: {}",
                    download.download_speed.as_deref().unwrap_or("n/a")
                )),
                Line::from(format!(
                    "Upload speed: {}",
                    download.upload_speed.as_deref().unwrap_or("n/a")
                )),
                Line::from(format!("Peers: {}", download.live_peers)),
                Line::from(format!("Connecting: {}", download.connecting_peers)),
                Line::from(format!("Seen peers: {}", download.seen_peers)),
                Line::from("Seeds: unavailable in current librqbit torrent stats"),
                Line::from(format!(
                    "Size: {}",
                    if download.total_bytes == 0 {
                        "unknown".to_string()
                    } else {
                        format_bytes(download.total_bytes)
                    }
                )),
                Line::from(format!(
                    "Error: {}",
                    download.error.as_deref().unwrap_or("none")
                )),
            ]))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Stats").borders(Borders::ALL));

            frame.render_widget(stats, right[1]);

            let details = Paragraph::new(Text::from(vec![
                Line::from(format!("Source: {}", download.source)),
                Line::from(format!("Output dir: {}", download.output_dir.display())),
            ]))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Paths").borders(Borders::ALL));

            frame.render_widget(details, right[2]);
        }
    }

    fn render_server(&self, frame: &mut Frame, area: Rect) {
        let lines = vec![
            Line::from("This screen keeps the integration points for server-side automation."),
            Line::from(""),
            Line::from(format!(
                "Data directory: {}",
                self.config.data_dir.display()
            )),
            Line::from(format!(
                "Default download directory: {}",
                self.config.default_download_dir.display()
            )),
            Line::from(format!(
                "Incoming .torrent directory: {}",
                self.config.incoming_torrents_dir.display()
            )),
            Line::from(""),
            Line::from("Next step: a Telegram bot can save received .torrent files here"),
            Line::from("and call the same TorrentEngine::add_torrent_source API."),
        ];

        let text = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Server / Bot").borders(Borders::ALL));

        frame.render_widget(text, area);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let help = match self.current_screen() {
            Screen::AddTorrent => {
                "Left/Right focus | Tab local completion | Up/Down switch field | Enter add torrent | Esc menu | Ctrl+C/F10 exit"
            }
            Screen::Downloads => {
                "Left/Right focus | Up/Down select torrent | Esc menu | Ctrl+C/F10 exit"
            }
            Screen::Server => "Left/Right focus | Esc menu | Ctrl+C/F10 exit",
        };

        let footer = Paragraph::new(Text::from(vec![
            Line::from(self.status_line.as_str()),
            Line::from(help),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Status").borders(Borders::ALL));

        frame.render_widget(footer, area);
    }

    fn render_input(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        value: &str,
        active: bool,
        placeholder: &str,
    ) {
        let border_style = if active {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let content = if value.is_empty() {
            Line::from(Span::styled(
                placeholder,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))
        } else {
            Line::from(value.to_string())
        };

        let input = Paragraph::new(content)
            .block(
                Block::default()
                    .title(title)
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
            .wrap(Wrap { trim: false });

        frame.render_widget(input, area);
    }
}

impl FormField {
    fn next(self) -> Self {
        match self {
            Self::TorrentSource => Self::DownloadDir,
            Self::DownloadDir => Self::TorrentSource,
        }
    }

    fn previous(self) -> Self {
        self.next()
    }

    fn title(self) -> &'static str {
        match self {
            Self::TorrentSource => "Torrent source",
            Self::DownloadDir => "Output directory",
        }
    }

    fn short_title(self) -> &'static str {
        match self {
            Self::TorrentSource => "Source",
            Self::DownloadDir => "Directory",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::TorrentSource => {
                "Example: ./movie.torrent, https://site/file.torrent, or magnet:?..."
            }
            Self::DownloadDir => "Example: ~/downloads",
        }
    }

    fn completion_subject(self) -> &'static str {
        match self {
            Self::TorrentSource => "source path",
            Self::DownloadDir => "directory path",
        }
    }

    fn completion_mode(self) -> PathCompletionMode {
        match self {
            Self::TorrentSource => PathCompletionMode::TorrentFile,
            Self::DownloadDir => PathCompletionMode::Directory,
        }
    }

    fn supports_local_completion(self, current_input: &str) -> bool {
        match self {
            Self::TorrentSource => TorrentSource::supports_local_completion(current_input),
            Self::DownloadDir => true,
        }
    }
}

impl Screen {
    fn all() -> [Screen; 3] {
        [Self::AddTorrent, Self::Downloads, Self::Server]
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Downloads,
            2 => Self::Server,
            _ => Self::AddTorrent,
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    fn title(self) -> &'static str {
        match self {
            Self::AddTorrent => "Choose torrent source",
            Self::Downloads => "Downloads",
            Self::Server => "Server",
        }
    }
}

fn completion_window(total_items: usize, selected: Option<usize>) -> (usize, usize) {
    if total_items <= MAX_VISIBLE_COMPLETIONS {
        return (0, total_items);
    }

    let Some(selected) = selected else {
        return (0, MAX_VISIBLE_COMPLETIONS);
    };

    let mut start = selected.saturating_sub(MAX_VISIBLE_COMPLETIONS / 2);
    let mut end = start + MAX_VISIBLE_COMPLETIONS;

    if end > total_items {
        end = total_items;
        start = end - MAX_VISIBLE_COMPLETIONS;
    }

    (start, end)
}
