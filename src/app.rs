use crate::{
    config::AppConfig,
    path_completion::{
        collect_candidates, resolve_user_path, CompletionCandidate, CompletionSet,
        PathCompletionMode,
    },
    torrent::{format_bytes, TorrentEngine, TorrentSnapshot, TorrentSource},
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use std::{
    collections::HashSet,
    env,
    path::Path,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

const TICK_RATE: Duration = Duration::from_millis(200);
const MAX_VISIBLE_COMPLETIONS: usize = 8;

pub struct App {
    config: AppConfig,
    engine: TorrentEngine,
    downloads: Vec<TorrentSnapshot>,
    total_downloads: usize,
    menu_state: ListState,
    downloads_state: ListState,
    focus: FocusArea,
    form_field: FormField,
    torrent_source: String,
    download_dir: String,
    completion_state: Option<CompletionState>,
    downloads_view: DownloadsView,
    download_action: DownloadAction,
    pending_cancellations: HashSet<usize>,
    event_tx: Sender<AppEvent>,
    event_rx: Receiver<AppEvent>,
    modal: Option<ModalState>,
    status_line: String,
    should_quit: bool,
}

#[derive(Debug)]
enum AppEvent {
    CancelCompleted {
        id: usize,
        name: String,
        result: std::result::Result<(), String>,
    },
}

#[derive(Clone, Debug)]
struct CompletionState {
    field: FormField,
    seed_input: String,
    candidates: Vec<CompletionCandidate>,
    selected: usize,
}

impl CompletionState {
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
        self.field == field && !self.candidates.is_empty() && current_input == self.seed_input
    }
}

#[derive(Clone, Debug)]
struct CompletionPreview {
    candidates: Vec<CompletionCandidate>,
    selected: Option<usize>,
    start_index: usize,
    total_matches: usize,
}

#[derive(Clone, Debug, Default)]
struct DownloadsView {
    filter_query: String,
    sort_field: DownloadSortField,
    sort_direction: SortDirection,
    display_mode: DownloadDisplayMode,
}

impl DownloadsView {
    fn filter_summary(&self) -> String {
        let filter = self.filter_query.trim();
        if filter.is_empty() {
            "all torrents".to_string()
        } else {
            format!("\"{filter}\"")
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Clone, Debug)]
enum ModalState {
    Help,
    FilterInput {
        value: String,
    },
    SortPicker {
        selected: usize,
        direction: SortDirection,
    },
    Confirm(ConfirmState),
}

#[derive(Clone, Debug)]
struct ConfirmState {
    action: ConfirmAction,
    selected: ConfirmChoice,
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DownloadSortField {
    #[default]
    Added,
    Speed,
    Progress,
    Name,
    State,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DownloadDisplayMode {
    Compact,
    #[default]
    Expanded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmAction {
    ExitApp,
    ClearFilter,
    ResetDownloadsView,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmChoice {
    Confirm,
    Cancel,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum DownloadAction {
    #[default]
    Stop,
    Resume,
    Cancel,
}

impl App {
    pub fn new(config: AppConfig, engine: TorrentEngine) -> Self {
        let mut menu_state = ListState::default();
        menu_state.select(Some(Screen::AddTorrent.index()));
        let (event_tx, event_rx) = mpsc::channel();

        Self {
            download_dir: config.default_download_dir.display().to_string(),
            status_line: format!(
                "Incoming folder reserved for a future Telegram bot: {}",
                config.incoming_torrents_dir.display()
            ),
            config,
            engine,
            downloads: Vec::new(),
            total_downloads: 0,
            menu_state,
            downloads_state: ListState::default(),
            focus: FocusArea::Content,
            form_field: FormField::TorrentSource,
            torrent_source: String::new(),
            completion_state: None,
            downloads_view: DownloadsView::default(),
            download_action: DownloadAction::default(),
            pending_cancellations: HashSet::new(),
            event_tx,
            event_rx,
            modal: None,
            should_quit: false,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        while !self.should_quit {
            self.handle_app_events();
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

    fn handle_app_events(&mut self) {
        while let Ok(AppEvent::CancelCompleted { id, name, result }) = self.event_rx.try_recv() {
            self.pending_cancellations.remove(&id);
            self.focus = FocusArea::Content;
            self.modal = None;

            match result {
                Ok(()) => {
                    self.status_line = format!("Cancelled torrent #{id}: {name}");
                }
                Err(error) => {
                    self.status_line = format!("Failed to cancel torrent #{id}: {error}");
                }
            }
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if matches!(key.code, KeyCode::F(10))
            || (key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c')))
        {
            self.should_quit = true;
            return Ok(());
        }

        if self.modal.is_some() {
            return self.handle_modal_key(key).await;
        }

        if matches!(key.code, KeyCode::F(1)) {
            self.open_help_popup();
            return Ok(());
        }

        if matches!(key.code, KeyCode::Char('q'))
            && !(self.focus == FocusArea::Content && self.current_screen() == Screen::AddTorrent)
        {
            self.open_confirm(ConfirmAction::ExitApp);
            return Ok(());
        }

        match self.focus {
            FocusArea::Menu => match key.code {
                KeyCode::Right => {
                    self.focus = FocusArea::Content;
                    Ok(())
                }
                KeyCode::Esc => Ok(()),
                _ => {
                    self.handle_menu_key(key);
                    Ok(())
                }
            },
            FocusArea::Content => match self.current_screen() {
                Screen::AddTorrent => match key.code {
                    KeyCode::Esc => {
                        self.focus = FocusArea::Menu;
                        Ok(())
                    }
                    _ => self.handle_add_torrent_key(key).await,
                },
                Screen::Downloads => match key.code {
                    KeyCode::Esc => {
                        self.focus = FocusArea::Menu;
                        Ok(())
                    }
                    _ => self.handle_downloads_key(key).await,
                },
                Screen::Server => match key.code {
                    KeyCode::Left | KeyCode::Esc => {
                        self.focus = FocusArea::Menu;
                        Ok(())
                    }
                    _ => Ok(()),
                },
            },
        }
    }

    async fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.modal.take() else {
            return Ok(());
        };

        match modal {
            ModalState::Help => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::F(1) => {
                    self.status_line = "Help closed".to_string();
                }
                _ => self.modal = Some(ModalState::Help),
            },
            ModalState::FilterInput { mut value } => match key.code {
                KeyCode::Esc => {
                    self.status_line = "Filter update cancelled".to_string();
                }
                KeyCode::Enter => {
                    self.downloads_view.filter_query = value.trim().to_string();
                    self.refresh_downloads();
                    if self.downloads_view.filter_query.is_empty() {
                        self.status_line = format!(
                            "Downloads filter cleared: showing {}/{} torrents",
                            self.downloads.len(),
                            self.total_downloads
                        );
                    } else {
                        self.status_line = format!(
                            "Downloads filter set to {}: showing {}/{} torrents",
                            self.downloads_view.filter_summary(),
                            self.downloads.len(),
                            self.total_downloads
                        );
                    }
                }
                KeyCode::Backspace => {
                    value.pop();
                    self.modal = Some(ModalState::FilterInput { value });
                }
                KeyCode::Delete => {
                    value.clear();
                    self.modal = Some(ModalState::FilterInput { value });
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    value.clear();
                    self.modal = Some(ModalState::FilterInput { value });
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    value.push(ch);
                    self.modal = Some(ModalState::FilterInput { value });
                }
                _ => self.modal = Some(ModalState::FilterInput { value }),
            },
            ModalState::SortPicker {
                mut selected,
                mut direction,
            } => {
                let options = DownloadSortField::all();
                match key.code {
                    KeyCode::Esc => {
                        self.status_line = "Sort dialog closed".to_string();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        selected = selected.saturating_sub(1);
                        self.modal = Some(ModalState::SortPicker {
                            selected,
                            direction,
                        });
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(options.len() - 1);
                        self.modal = Some(ModalState::SortPicker {
                            selected,
                            direction,
                        });
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Char('r') => {
                        direction.toggle();
                        self.modal = Some(ModalState::SortPicker {
                            selected,
                            direction,
                        });
                    }
                    KeyCode::Enter => {
                        let field = options[selected];
                        self.downloads_view.sort_field = field;
                        self.downloads_view.sort_direction = direction;
                        self.refresh_downloads();
                        self.status_line = format!(
                            "Downloads sorted by {} ({})",
                            field.title(),
                            direction.title()
                        );
                    }
                    _ => {
                        self.modal = Some(ModalState::SortPicker {
                            selected,
                            direction,
                        });
                    }
                }
            }
            ModalState::Confirm(mut confirm) => match key.code {
                KeyCode::Esc => {
                    self.status_line = format!("{} cancelled", confirm.action.title());
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    confirm.selected.toggle();
                    self.modal = Some(ModalState::Confirm(confirm));
                }
                KeyCode::Enter => {
                    if confirm.selected == ConfirmChoice::Confirm {
                        self.apply_confirm_action(confirm.action).await;
                    } else {
                        self.status_line = format!("{} cancelled", confirm.action.title());
                    }
                }
                KeyCode::Char('y') => self.apply_confirm_action(confirm.action).await,
                KeyCode::Char('n') => {
                    self.status_line = format!("{} cancelled", confirm.action.title());
                }
                _ => self.modal = Some(ModalState::Confirm(confirm)),
            },
        }

        Ok(())
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_screen(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_screen(),
            KeyCode::Home => self.set_screen(Screen::AddTorrent),
            KeyCode::End => self.set_screen(Screen::Server),
            KeyCode::Enter => self.focus = FocusArea::Content,
            _ => {}
        }
    }

    async fn handle_add_torrent_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up => self.move_browser_selection(CompletionDirection::Backward)?,
            KeyCode::Down => self.move_browser_selection(CompletionDirection::Forward)?,
            KeyCode::Left => self.browse_parent_directory()?,
            KeyCode::F(5) => self.submit_torrent().await?,
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.submit_torrent().await?
            }
            KeyCode::Tab => self.switch_active_form_field(self.form_field.next()),
            KeyCode::BackTab => self.switch_active_form_field(self.form_field.previous()),
            KeyCode::Backspace => self.pop_active_input(),
            KeyCode::Delete => self.clear_active_input(),
            KeyCode::Char(' ') => self.handle_add_selection().await?,
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

    fn switch_active_form_field(&mut self, field: FormField) {
        self.clear_completion();
        self.form_field = field;
        self.status_line = format!("Active field: {}", self.form_field.title());
    }

    async fn handle_add_selection(&mut self) -> Result<()> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim().to_string();

        if field == FormField::TorrentSource
            && !current_input.is_empty()
            && !field.supports_local_completion(&current_input)
        {
            self.submit_torrent().await?;
            return Ok(());
        }

        let Some(state) = self.load_browser_state(field, &current_input)? else {
            let can_submit_local_file = field == FormField::TorrentSource
                && resolve_user_path(&current_input)
                    .map(|path| path.is_file())
                    .unwrap_or(false);

            if can_submit_local_file {
                self.submit_torrent().await?;
            } else {
                self.status_line = format!("No {} entries found", field.browser_subject());
            }
            return Ok(());
        };

        let selected = &state.candidates[state.selected];
        if field == FormField::TorrentSource
            && !selected.is_dir
            && !selected.is_remote_hint
            && selected.replacement == current_input
        {
            self.submit_torrent().await?;
            return Ok(());
        }

        self.activate_browser_selection()
    }

    async fn handle_downloads_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_download(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_download(),
            KeyCode::Home | KeyCode::Char('g') => self.select_first_download(),
            KeyCode::End | KeyCode::Char('G') => self.select_last_download(),
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(download) = self.selected_download() {
                    self.download_action = self.download_action.previous_available(download);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(download) = self.selected_download() {
                    self.download_action = self.download_action.next_available(download);
                }
            }
            KeyCode::Char(' ') => self.execute_selected_download_action().await?,
            KeyCode::Char('/') => self.open_filter_dialog(),
            KeyCode::Char('s') => self.open_sort_dialog(),
            KeyCode::Char('r') => self.reverse_sort_direction(),
            KeyCode::Char('m') => self.toggle_download_display_mode(),
            KeyCode::Char('c') => self.confirm_clear_filter(),
            KeyCode::Char('x') => self.open_confirm(ConfirmAction::ResetDownloadsView),
            _ => {}
        }

        self.normalize_download_action();
        Ok(())
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

    fn move_browser_selection(&mut self, direction: CompletionDirection) -> Result<()> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim().to_string();

        let has_existing_state = self
            .completion_state
            .as_ref()
            .is_some_and(|state| state.can_continue(field, &current_input));

        let Some(mut state) = self.load_browser_state(field, &current_input)? else {
            self.clear_completion();
            self.status_line = format!("No {} entries found", field.browser_subject());
            return Ok(());
        };

        if has_existing_state {
            state.step(direction);
        } else {
            state.selected = match direction {
                CompletionDirection::Forward => (state.candidates.len() > 1) as usize,
                CompletionDirection::Backward => state.candidates.len() - 1,
            };
        }
        let total = state.candidates.len();
        let index = state.selected + 1;
        self.completion_state = Some(state);
        self.status_line = format!("{} browser {index}/{total}", field.browser_subject(),);

        Ok(())
    }

    fn activate_browser_selection(&mut self) -> Result<()> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim().to_string();

        let Some(state) = self.load_browser_state(field, &current_input)? else {
            self.clear_completion();
            self.status_line = format!("No {} entries found", field.browser_subject());
            return Ok(());
        };

        let selected = &state.candidates[state.selected];
        let replacement = selected.replacement.clone();
        let is_dir = selected.is_dir;

        if selected.is_remote_hint {
            self.set_active_value(field, String::new());
            self.clear_completion();
            self.status_line = "Paste a URL or magnet link into Source URL/file".to_string();
            return Ok(());
        }

        self.set_active_value(field, replacement.clone());
        self.status_line = if is_dir {
            self.completion_state = self.load_browser_state(field, &replacement)?;
            format!("Opened {}", replacement)
        } else {
            self.clear_completion();
            format!("Selected torrent file: {}", replacement)
        };

        Ok(())
    }

    fn browse_parent_directory(&mut self) -> Result<()> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim();

        if !field.supports_local_completion(current_input) {
            self.clear_completion();
            self.status_line =
                "Clear the source field to browse directories or keep pasting a URL/magnet"
                    .to_string();
            return Ok(());
        }

        let parent = parent_input_path(current_input)?;
        self.set_active_value(field, parent.clone());
        self.clear_completion();
        self.status_line = format!("Moved to parent: {parent}");
        Ok(())
    }

    fn load_browser_state(
        &self,
        field: FormField,
        current_input: &str,
    ) -> Result<Option<CompletionState>> {
        if let Some(state) = &self.completion_state {
            if state.can_continue(field, current_input) {
                return Ok(Some(state.clone()));
            }
        }

        let completion_set = self.build_completion_set(field, current_input)?;
        if completion_set.candidates.is_empty() {
            return Ok(None);
        }

        Ok(Some(CompletionState {
            field,
            seed_input: completion_set.seed_input,
            candidates: completion_set.candidates,
            selected: 0,
        }))
    }

    fn build_completion_set(&self, field: FormField, current_input: &str) -> Result<CompletionSet> {
        if field == FormField::TorrentSource
            && (current_input.is_empty() || !field.supports_local_completion(current_input))
        {
            let mut completion = collect_candidates("", PathCompletionMode::Directory)?;
            completion.seed_input = current_input.to_string();
            completion
                .candidates
                .insert(0, CompletionCandidate::remote_hint());
            return Ok(completion);
        }

        collect_candidates(self.active_value(field), field.completion_mode())
    }

    fn refresh_downloads(&mut self) {
        let selected_id = self.selected_download_id();
        let filter_terms = self.download_filter_terms();
        let mut downloads = self.engine.list_downloads();
        self.total_downloads = downloads.len();
        downloads.retain(|download| Self::matches_download_filter(download, &filter_terms));
        self.sort_downloads(&mut downloads);
        self.downloads = downloads;

        if self.downloads.is_empty() {
            self.downloads_state.select(None);
            return;
        }

        let selected = selected_id
            .and_then(|id| self.downloads.iter().position(|download| download.id == id))
            .unwrap_or(0);
        self.downloads_state.select(Some(selected));
        self.normalize_download_action();
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
        let sanitized = text.trim_matches(|character| character == '\n' || character == '\r');
        if sanitized.is_empty() {
            return;
        }

        if let Some(modal) = self.modal.take() {
            match modal {
                ModalState::FilterInput { mut value } => {
                    value.push_str(sanitized);
                    self.status_line = "Pasted into downloads filter".to_string();
                    self.modal = Some(ModalState::FilterInput { value });
                }
                other => {
                    self.modal = Some(other);
                }
            }
            return;
        }

        if self.focus != FocusArea::Content || self.current_screen() != Screen::AddTorrent {
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

    fn selected_download_id(&self) -> Option<usize> {
        self.downloads_state
            .selected()
            .and_then(|index| self.downloads.get(index))
            .map(|download| download.id)
    }

    fn selected_download(&self) -> Option<&TorrentSnapshot> {
        self.downloads_state
            .selected()
            .and_then(|index| self.downloads.get(index))
    }

    fn normalize_download_action(&mut self) {
        if let Some(download) = self.selected_download() {
            if self.pending_cancellations.contains(&download.id)
                || !self.download_action.is_available(download)
            {
                self.download_action = DownloadAction::preferred_for(download);
            }
        }
    }

    fn select_previous_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        let current = self.downloads_state.selected().unwrap_or(0);
        self.downloads_state.select(Some(current.saturating_sub(1)));
        self.normalize_download_action();
    }

    fn select_next_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        let current = self.downloads_state.selected().unwrap_or(0);
        let next = (current + 1).min(self.downloads.len() - 1);
        self.downloads_state.select(Some(next));
        self.normalize_download_action();
    }

    fn select_first_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        self.downloads_state.select(Some(0));
        self.normalize_download_action();
    }

    fn select_last_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        self.downloads_state.select(Some(self.downloads.len() - 1));
        self.normalize_download_action();
    }

    async fn execute_selected_download_action(&mut self) -> Result<()> {
        let Some(download) = self.selected_download().cloned() else {
            self.status_line = "No torrent selected".to_string();
            return Ok(());
        };

        if self.pending_cancellations.contains(&download.id) {
            self.status_line = format!(
                "Cancellation is already in progress for torrent #{}: {}",
                download.id, download.name
            );
            return Ok(());
        }

        if !self.download_action.is_available(&download) {
            self.status_line = format!(
                "{} is unavailable for {}",
                self.download_action.title(),
                download.name
            );
            return Ok(());
        }

        match self.download_action {
            DownloadAction::Stop => match self.engine.stop_download(download.id).await {
                Ok(()) => {
                    self.status_line =
                        format!("Stopped torrent #{}: {}", download.id, download.name);
                    self.refresh_downloads();
                    self.normalize_download_action();
                    self.focus = FocusArea::Content;
                }
                Err(error) => {
                    self.status_line =
                        format!("Failed to stop torrent #{}: {error:#}", download.id);
                }
            },
            DownloadAction::Resume => match self.engine.resume_download(download.id).await {
                Ok(()) => {
                    self.status_line =
                        format!("Resumed torrent #{}: {}", download.id, download.name);
                    self.refresh_downloads();
                    self.normalize_download_action();
                    self.focus = FocusArea::Content;
                }
                Err(error) => {
                    self.status_line =
                        format!("Failed to resume torrent #{}: {error:#}", download.id);
                }
            },
            DownloadAction::Cancel => {
                let id = download.id;
                let name = download.name;
                let engine = self.engine.clone();
                let tx = self.event_tx.clone();

                self.pending_cancellations.insert(id);
                self.focus = FocusArea::Content;
                self.status_line = format!("Cancelling torrent #{id}: {name}");

                tokio::spawn(async move {
                    let result = engine
                        .cancel_download(id)
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let _ = tx.send(AppEvent::CancelCompleted { id, name, result });
                });
            }
        }

        Ok(())
    }

    fn open_help_popup(&mut self) {
        self.modal = Some(ModalState::Help);
        self.status_line = "Help opened".to_string();
    }

    fn open_filter_dialog(&mut self) {
        self.modal = Some(ModalState::FilterInput {
            value: self.downloads_view.filter_query.clone(),
        });
        self.status_line = "Editing downloads filter".to_string();
    }

    fn open_sort_dialog(&mut self) {
        let selected = DownloadSortField::all()
            .iter()
            .position(|field| *field == self.downloads_view.sort_field)
            .unwrap_or(0);
        self.modal = Some(ModalState::SortPicker {
            selected,
            direction: self.downloads_view.sort_direction,
        });
        self.status_line = "Choose downloads sort".to_string();
    }

    fn open_confirm(&mut self, action: ConfirmAction) {
        self.modal = Some(ModalState::Confirm(ConfirmState {
            action,
            selected: ConfirmChoice::Cancel,
        }));
        self.status_line = action.prompt().to_string();
    }

    fn reverse_sort_direction(&mut self) {
        self.downloads_view.sort_direction.toggle();
        self.refresh_downloads();
        self.status_line = format!(
            "Sort order switched to {} for {}",
            self.downloads_view.sort_direction.title(),
            self.downloads_view.sort_field.title()
        );
    }

    fn toggle_download_display_mode(&mut self) {
        self.downloads_view.display_mode.toggle();
        self.status_line = format!(
            "Downloads view switched to {} mode",
            self.downloads_view.display_mode.title()
        );
    }

    fn confirm_clear_filter(&mut self) {
        if self.downloads_view.filter_query.trim().is_empty() {
            self.status_line = "Downloads filter is already empty".to_string();
            return;
        }

        self.open_confirm(ConfirmAction::ClearFilter);
    }

    async fn apply_confirm_action(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::ExitApp => {
                self.status_line = "Closing rus-torrent".to_string();
                self.should_quit = true;
            }
            ConfirmAction::ClearFilter => {
                self.downloads_view.filter_query.clear();
                self.refresh_downloads();
                self.status_line = format!(
                    "Downloads filter cleared: showing {}/{} torrents",
                    self.downloads.len(),
                    self.total_downloads
                );
            }
            ConfirmAction::ResetDownloadsView => {
                self.downloads_view.reset();
                self.refresh_downloads();
                self.status_line = format!(
                    "Downloads view reset to defaults: showing {}/{} torrents",
                    self.downloads.len(),
                    self.total_downloads
                );
            }
        }
    }

    fn download_filter_terms(&self) -> Vec<String> {
        self.downloads_view
            .filter_query
            .split_whitespace()
            .map(|term| term.to_ascii_lowercase())
            .collect()
    }

    fn matches_download_filter(download: &TorrentSnapshot, filter_terms: &[String]) -> bool {
        if filter_terms.is_empty() {
            return true;
        }

        let haystack = format!(
            "{} {} {} {}",
            download.name.to_ascii_lowercase(),
            download.state.to_ascii_lowercase(),
            download.source.to_ascii_lowercase(),
            download
                .output_dir
                .display()
                .to_string()
                .to_ascii_lowercase()
        );

        filter_terms
            .iter()
            .all(|term| haystack.contains(term.as_str()))
    }

    fn sort_downloads(&self, downloads: &mut [TorrentSnapshot]) {
        downloads.sort_by(|left, right| {
            let ordering = match self.downloads_view.sort_field {
                DownloadSortField::Added => left.id.cmp(&right.id),
                DownloadSortField::Speed => {
                    left.download_speed_mib.total_cmp(&right.download_speed_mib)
                }
                DownloadSortField::Progress => left.progress_ratio.total_cmp(&right.progress_ratio),
                DownloadSortField::Name => left
                    .name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase()),
                DownloadSortField::State => left
                    .state
                    .to_ascii_lowercase()
                    .cmp(&right.state.to_ascii_lowercase()),
            };

            let ordering = match self.downloads_view.sort_direction {
                SortDirection::Ascending => ordering,
                SortDirection::Descending => ordering.reverse(),
            };

            ordering.then_with(|| left.id.cmp(&right.id))
        });
    }

    fn completion_preview(&self) -> Result<CompletionPreview> {
        let field = self.form_field;
        let current_input = self.active_value(field).trim();

        let (candidates, selected) = match &self.completion_state {
            Some(state) if state.can_continue(field, current_input) => {
                (state.candidates.clone(), Some(state.selected))
            }
            _ => {
                let completion = self.build_completion_set(field, current_input)?;
                let selected = (!completion.candidates.is_empty()).then_some(0);
                (completion.candidates, selected)
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
        let current_input = self.active_value(self.form_field).trim();

        match self.form_field {
            FormField::TorrentSource
                if current_input.is_empty()
                    || !self.form_field.supports_local_completion(current_input) =>
            {
                Text::from(vec![
                    Line::from("Choose URL/magnet or open one of your home directories."),
                    Line::from("Space opens the highlighted choice."),
                ])
            }
            FormField::TorrentSource => Text::from(vec![
                Line::from("No directories or .torrent files match the current source path."),
                Line::from("Space opens a directory or selects the highlighted torrent file."),
            ]),
            FormField::DownloadDir => Text::from(vec![
                Line::from("No directories match the current input."),
                Line::from("Space opens the selected directory."),
            ]),
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
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
        self.render_modal(frame);
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let focus = match self.focus {
            FocusArea::Menu => "menu",
            FocusArea::Content => "content",
        };

        let mut spans = vec![
            Span::styled(
                "rus-torrent",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("screen: {}", self.current_screen().title()),
                Style::default().fg(Color::White),
            ),
            Span::raw("  "),
            Span::styled(
                format!("focus: {focus}"),
                Style::default().fg(Color::Yellow),
            ),
        ];

        if let Some(title) = self.active_modal_title() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                format!("popup: {title}"),
                Style::default().fg(Color::Magenta),
            ));
        }

        let header = Paragraph::new(Line::from(spans))
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
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(chunks[2]);

        self.render_completion_panel(frame, bottom[0]);
        self.render_add_summary(frame, bottom[1]);
    }

    fn render_completion_panel(&self, frame: &mut Frame, area: Rect) {
        let preview = self.completion_preview();
        match preview {
            Ok(preview) if preview.candidates.is_empty() => {
                let empty = Paragraph::new(self.completion_empty_text())
                    .wrap(Wrap { trim: true })
                    .block(
                        Block::default()
                            .title(self.completion_panel_title())
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
                                Style::default().fg(if candidate.is_remote_hint {
                                    Color::Magenta
                                } else if candidate.is_dir {
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
                        "{} (showing {}-{} of {})",
                        self.completion_panel_title(),
                        preview.start_index + 1,
                        preview.start_index + preview.candidates.len(),
                        preview.total_matches
                    )
                } else {
                    format!(
                        "{} ({})",
                        self.completion_panel_title(),
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
                    Line::from(self.completion_unavailable_text()),
                    Line::from(format!("{error:#}")),
                ]))
                .wrap(Wrap { trim: true })
                .block(
                    Block::default()
                        .title(self.completion_panel_title())
                        .borders(Borders::ALL),
                );
                frame.render_widget(message, area);
            }
        }
    }

    fn completion_panel_title(&self) -> &'static str {
        match self.form_field {
            FormField::TorrentSource => "Source URL/file",
            FormField::DownloadDir => "Choose directory",
        }
    }

    fn completion_unavailable_text(&self) -> &'static str {
        match self.form_field {
            FormField::TorrentSource => "Source chooser is currently unavailable.",
            FormField::DownloadDir => "Directory chooser is currently unavailable.",
        }
    }

    fn render_add_summary(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(11), Constraint::Min(0)])
            .split(area);

        let source_summary = describe_torrent_source(&self.torrent_source);
        let output_summary = describe_output_directory(&self.download_dir);
        let active_style = active_field_style(self.form_field);
        let source_kind_style = source_kind_style(&source_summary);
        let source_value_style = source_value_style(&source_summary);
        let output_value_style = output_value_style(self.download_dir.trim());

        let selection = Paragraph::new(Text::from(vec![
            Line::from(vec![
                Span::styled("Active ", active_style),
                Span::styled(
                    self.form_field.title(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "Source ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    source_summary.kind,
                    source_kind_style.add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                source_summary.value.as_str(),
                source_value_style,
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Output directory",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(output_summary.as_str(), output_value_style)),
        ]))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(Line::from(vec![Span::styled(
                    " Selection ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )]))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

        frame.render_widget(selection, chunks[0]);

        let workspace = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Session folders on disk",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "Data ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.config.data_dir.display().to_string(),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    "Incoming ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.config.incoming_torrents_dir.display().to_string(),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "Downloads ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.config.default_download_dir.display().to_string(),
                    Style::default().fg(Color::White),
                ),
            ]),
        ]))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(Line::from(vec![Span::styled(
                    " Workspace ",
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )]))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );

        frame.render_widget(workspace, chunks[1]);
    }

    fn render_downloads(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        self.render_downloads_toolbar(frame, chunks[0]);

        if self.downloads.is_empty() {
            self.render_downloads_empty(frame, chunks[1]);
            return;
        }

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(match self.downloads_view.display_mode {
                DownloadDisplayMode::Compact => {
                    [Constraint::Percentage(46), Constraint::Percentage(54)]
                }
                DownloadDisplayMode::Expanded => {
                    [Constraint::Percentage(52), Constraint::Percentage(48)]
                }
            })
            .split(chunks[1]);

        let items = self
            .downloads
            .iter()
            .map(|download| match self.downloads_view.display_mode {
                DownloadDisplayMode::Compact => ListItem::new(Line::from(vec![
                    Span::styled(
                        download.name.as_str(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        "  #{:02}  {:.1}%  {}  ↓ {}  peers {}",
                        download.id,
                        download.progress_ratio * 100.0,
                        download.state,
                        download.download_speed.as_deref().unwrap_or("n/a"),
                        download.live_peers
                    )),
                ])),
                DownloadDisplayMode::Expanded => ListItem::new(vec![
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
                ]),
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
                    .title(self.downloads_list_title())
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

        frame.render_stateful_widget(list, body[0], &mut self.downloads_state);

        let selected = self
            .downloads_state
            .selected()
            .and_then(|index| self.downloads.get(index));

        if let Some(download) = selected {
            let right = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(3),
                    Constraint::Length(13),
                    Constraint::Min(0),
                ])
                .split(body[1]);

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

            self.render_download_actions(frame, right[1], download);

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

            frame.render_widget(stats, right[2]);

            let details = Paragraph::new(Text::from(vec![
                Line::from(format!("Source: {}", download.source)),
                Line::from(format!("Output dir: {}", download.output_dir.display())),
            ]))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Paths").borders(Borders::ALL));

            frame.render_widget(details, right[3]);
        }
    }

    fn render_download_actions(&self, frame: &mut Frame, area: Rect, download: &TorrentSnapshot) {
        let cancel_pending = self.pending_cancellations.contains(&download.id);
        let spans = DownloadAction::all()
            .iter()
            .flat_map(|action| {
                let title = if cancel_pending && *action == DownloadAction::Cancel {
                    " Cancelling... ".to_string()
                } else {
                    format!(" {} ", action.title())
                };

                [
                    Span::styled(
                        title,
                        action.button_style(
                            self.download_action == *action,
                            download,
                            cancel_pending,
                        ),
                    ),
                    Span::raw(" "),
                ]
            })
            .collect::<Vec<_>>();

        let buttons = Paragraph::new(Line::from(spans))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Actions").borders(Borders::ALL));

        frame.render_widget(buttons, area);
    }

    fn render_downloads_toolbar(&self, frame: &mut Frame, area: Rect) {
        let summary = Paragraph::new(Line::from(vec![
            Span::styled("Visible ", Style::default().fg(Color::Cyan)),
            Span::raw(format!("{}/{}", self.downloads.len(), self.total_downloads)),
            Span::raw("  "),
            Span::styled("Filter ", Style::default().fg(Color::Cyan)),
            Span::raw(self.downloads_view.filter_summary()),
            Span::raw("  "),
            Span::styled("Sort ", Style::default().fg(Color::Cyan)),
            Span::raw(format!(
                "{} ({})",
                self.downloads_view.sort_field.title(),
                self.downloads_view.sort_direction.title()
            )),
            Span::raw("  "),
            Span::styled("Mode ", Style::default().fg(Color::Cyan)),
            Span::raw(self.downloads_view.display_mode.title()),
        ]))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title("Downloads View")
                .borders(Borders::ALL),
        );

        frame.render_widget(summary, area);
    }

    fn render_downloads_empty(&self, frame: &mut Frame, area: Rect) {
        let text = if self.total_downloads == 0 {
            Text::from(vec![
                Line::from("No torrents have been added yet."),
                Line::from("Queue a source from the add screen to start the session."),
            ])
        } else {
            Text::from(vec![
                Line::from("No downloads match the current filter."),
                Line::from(format!(
                    "Current filter: {}",
                    self.downloads_view.filter_summary()
                )),
                Line::from("Adjust the filter to show hidden downloads."),
            ])
        };

        let empty = Paragraph::new(text)
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Downloads").borders(Borders::ALL));
        frame.render_widget(empty, area);
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
        let footer = Paragraph::new(Line::from(self.status_line.as_str()))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Status").borders(Borders::ALL));

        frame.render_widget(footer, area);
    }

    fn render_modal(&self, frame: &mut Frame) {
        match &self.modal {
            Some(ModalState::Help) => self.render_help_popup(frame),
            Some(ModalState::FilterInput { value }) => self.render_filter_popup(frame, value),
            Some(ModalState::SortPicker {
                selected,
                direction,
            }) => self.render_sort_popup(frame, *selected, *direction),
            Some(ModalState::Confirm(confirm)) => self.render_confirm_popup(frame, confirm),
            None => {}
        }
    }

    fn render_help_popup(&self, frame: &mut Frame) {
        let area = centered_rect(74, 72, frame.area());
        frame.render_widget(Clear, area);

        let popup = Paragraph::new(Text::from(vec![
            Line::styled(
                "Global",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("F1: open or close help"),
            Line::from("Left/Right: move focus between menu and content"),
            Line::from("Ctrl+C or F10: force exit immediately"),
            Line::from(""),
            Line::styled(
                "Source URL/file",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("Tab / Shift+Tab: switch active field"),
            Line::from("Up/Down: move browser selection"),
            Line::from("Left: parent directory"),
            Line::from("Space: open directory or select URL/magnet / torrent file"),
            Line::from("F5 or Ctrl+S: start download"),
            Line::from("Paste: insert file path, URL, or magnet link"),
            Line::from(""),
            Line::styled(
                "Downloads",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("Up/Down or j/k: select a torrent"),
            Line::from("Left/Right or h/l: choose Stop / Resume / Cancel"),
            Line::from("Home/End or g/G: jump to first or last torrent"),
            Line::from("Space: run the selected action"),
            Line::from("/: open search/filter dialog"),
            Line::from("s: open sort dialog"),
            Line::from("r: reverse current sort order"),
            Line::from("m: toggle compact and expanded list modes"),
            Line::from("c: clear filter with confirmation"),
            Line::from("x: reset filter, sort, and layout with confirmation"),
            Line::from("q: quit with confirmation"),
            Line::from(""),
            Line::styled(
                "Popups",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("Enter: apply or confirm"),
            Line::from("Esc: close or cancel"),
            Line::from("Filter dialog: Ctrl+U/Delete clears the current input"),
        ]))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title("Hotkeys and Popups")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );

        frame.render_widget(popup, area);
    }

    fn render_filter_popup(&self, frame: &mut Frame, value: &str) {
        let area = centered_rect(64, 34, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default()
                .title("Filter Downloads")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
            area,
        );

        let inner = inner_rect(area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(inner);

        let content = if value.is_empty() {
            Line::from(Span::styled(
                "Type terms to match name, state, source, or output directory",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ))
        } else {
            Line::from(value.to_string())
        };

        let input = Paragraph::new(content)
            .wrap(Wrap { trim: false })
            .block(Block::default().title("Query").borders(Borders::ALL));
        frame.render_widget(input, chunks[0]);

        let help = Paragraph::new(Text::from(vec![
            Line::from("Filtering is case-insensitive."),
            Line::from("Separate multiple terms with spaces; every term must match."),
        ]))
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title("Matching Rules")
                .borders(Borders::ALL),
        );
        frame.render_widget(help, chunks[1]);
    }

    fn render_sort_popup(&self, frame: &mut Frame, selected: usize, direction: SortDirection) {
        let area = centered_rect(64, 56, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default()
                .title("Sort Downloads")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
            area,
        );

        let inner = inner_rect(area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(inner);

        let order = Paragraph::new(Line::from(vec![
            Span::styled("Direction ", Style::default().fg(Color::Cyan)),
            Span::raw(direction.title()),
            Span::raw("  "),
            Span::styled("Current ", Style::default().fg(Color::Cyan)),
            Span::raw(self.downloads_view.sort_field.title()),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Order").borders(Borders::ALL));
        frame.render_widget(order, chunks[0]);

        let items = DownloadSortField::all()
            .iter()
            .map(|field| {
                let suffix = if *field == self.downloads_view.sort_field {
                    " (current)"
                } else {
                    ""
                };
                ListItem::new(vec![
                    Line::from(format!("{}{}", field.title(), suffix)),
                    Line::from(Span::styled(
                        field.description(),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect::<Vec<_>>();

        let mut list_state = ListState::default();
        list_state.select(Some(selected));

        let list = List::new(items)
            .block(Block::default().title("Sort Field").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        frame.render_stateful_widget(list, chunks[1], &mut list_state);
    }

    fn render_confirm_popup(&self, frame: &mut Frame, confirm: &ConfirmState) {
        let area = centered_rect(56, 28, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default()
                .title(confirm.action.title())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
            area,
        );

        let inner = inner_rect(area);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(inner);

        let message = Paragraph::new(self.confirm_text(confirm.action))
            .wrap(Wrap { trim: true })
            .block(Block::default().title("Confirm").borders(Borders::ALL));
        frame.render_widget(message, chunks[0]);

        let cancel_style = if confirm.selected == ConfirmChoice::Cancel {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Yellow)
        };

        let confirm_style = if confirm.selected == ConfirmChoice::Confirm {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };

        let buttons = Paragraph::new(Line::from(vec![
            Span::styled(" Cancel ", cancel_style),
            Span::raw("  "),
            Span::styled(
                format!(" {} ", confirm.action.confirm_label()),
                confirm_style,
            ),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Choice").borders(Borders::ALL));
        frame.render_widget(buttons, chunks[1]);
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

    fn downloads_list_title(&self) -> String {
        if self.total_downloads == self.downloads.len() {
            format!("Active Downloads ({})", self.downloads.len())
        } else {
            format!(
                "Active Downloads ({}/{} shown)",
                self.downloads.len(),
                self.total_downloads
            )
        }
    }

    fn active_modal_title(&self) -> Option<&'static str> {
        match &self.modal {
            Some(ModalState::Help) => Some("help"),
            Some(ModalState::FilterInput { .. }) => Some("filter"),
            Some(ModalState::SortPicker { .. }) => Some("sort"),
            Some(ModalState::Confirm(_)) => Some("confirm"),
            None => None,
        }
    }

    fn confirm_text(&self, action: ConfirmAction) -> Text<'static> {
        match action {
            ConfirmAction::ExitApp => Text::from(vec![
                Line::from("Close rus-torrent now?"),
                Line::from("The current in-memory session will stop with the process."),
            ]),
            ConfirmAction::ClearFilter => Text::from(vec![
                Line::from("Clear the current downloads filter?"),
                Line::from(format!(
                    "Current filter: {}",
                    self.downloads_view.filter_summary()
                )),
            ]),
            ConfirmAction::ResetDownloadsView => Text::from(vec![
                Line::from("Reset filter, sort order, and layout to defaults?"),
                Line::from("Defaults: added order, ascending, expanded mode."),
            ]),
        }
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
            Self::TorrentSource => "Source URL/file",
            Self::DownloadDir => "Output directory",
        }
    }

    fn placeholder(self) -> &'static str {
        match self {
            Self::TorrentSource => {
                "Example: ./movie.torrent, https://site/file.torrent, or magnet:?..."
            }
            Self::DownloadDir => "Example: /home/user/",
        }
    }

    fn browser_subject(self) -> &'static str {
        match self {
            Self::TorrentSource => "source entries",
            Self::DownloadDir => "directories",
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
            Self::AddTorrent => "Source URL/file",
            Self::Downloads => "Downloads",
            Self::Server => "Server",
        }
    }
}

impl DownloadSortField {
    fn all() -> [Self; 5] {
        [
            Self::Added,
            Self::Speed,
            Self::Progress,
            Self::Name,
            Self::State,
        ]
    }

    fn title(self) -> &'static str {
        match self {
            Self::Added => "Added order",
            Self::Speed => "Download speed",
            Self::Progress => "Progress",
            Self::Name => "Name",
            Self::State => "State",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Added => "Order by torrent id so the queue looks like submission history.",
            Self::Speed => "Highest or lowest live download speed first.",
            Self::Progress => "Sort by completed percentage.",
            Self::Name => "Alphabetical order by torrent name.",
            Self::State => "Group torrents by current runtime state text.",
        }
    }
}

impl SortDirection {
    fn toggle(&mut self) {
        *self = match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        };
    }

    fn title(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }
}

impl DownloadDisplayMode {
    fn toggle(&mut self) {
        *self = match self {
            Self::Compact => Self::Expanded,
            Self::Expanded => Self::Compact,
        };
    }

    fn title(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Expanded => "expanded",
        }
    }
}

impl DownloadAction {
    fn all() -> [Self; 3] {
        [Self::Stop, Self::Resume, Self::Cancel]
    }

    fn next(self) -> Self {
        match self {
            Self::Stop => Self::Resume,
            Self::Resume => Self::Cancel,
            Self::Cancel => Self::Stop,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Stop => Self::Cancel,
            Self::Resume => Self::Stop,
            Self::Cancel => Self::Resume,
        }
    }

    fn next_available(self, download: &TorrentSnapshot) -> Self {
        let mut action = self;

        for _ in 0..Self::all().len() {
            action = action.next();
            if action.is_available(download) {
                return action;
            }
        }

        self
    }

    fn previous_available(self, download: &TorrentSnapshot) -> Self {
        let mut action = self;

        for _ in 0..Self::all().len() {
            action = action.previous();
            if action.is_available(download) {
                return action;
            }
        }

        self
    }

    fn title(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Resume => "Resume",
            Self::Cancel => "Cancel",
        }
    }

    fn preferred_for(download: &TorrentSnapshot) -> Self {
        if download.finished {
            Self::Cancel
        } else if download.is_stopped() {
            Self::Resume
        } else {
            Self::Stop
        }
    }

    fn is_available(self, download: &TorrentSnapshot) -> bool {
        match self {
            Self::Stop => !download.finished && !download.is_stopped(),
            Self::Resume => !download.finished && download.is_stopped(),
            Self::Cancel => true,
        }
    }

    fn button_style(
        self,
        selected: bool,
        download: &TorrentSnapshot,
        cancel_pending: bool,
    ) -> Style {
        if cancel_pending {
            if self == Self::Cancel {
                return Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD);
            }

            return Style::default().fg(Color::DarkGray);
        }

        if !self.is_available(download) {
            return Style::default().fg(Color::DarkGray);
        }

        let base = match self {
            Self::Stop => Style::default().fg(Color::Yellow),
            Self::Resume => Style::default().fg(Color::Green),
            Self::Cancel => Style::default().fg(Color::Red),
        };

        if selected {
            match self {
                Self::Stop => Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
                Self::Resume => Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                Self::Cancel => Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            }
        } else {
            base
        }
    }
}

impl ConfirmAction {
    fn title(self) -> &'static str {
        match self {
            Self::ExitApp => "Exit Application",
            Self::ClearFilter => "Clear Filter",
            Self::ResetDownloadsView => "Reset Downloads View",
        }
    }

    fn confirm_label(self) -> &'static str {
        match self {
            Self::ExitApp => "Exit",
            Self::ClearFilter => "Clear",
            Self::ResetDownloadsView => "Reset",
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Self::ExitApp => "Exit confirmation opened",
            Self::ClearFilter => "Clear filter confirmation opened",
            Self::ResetDownloadsView => "Reset view confirmation opened",
        }
    }
}

impl ConfirmChoice {
    fn toggle(&mut self) {
        *self = match self {
            Self::Confirm => Self::Cancel,
            Self::Cancel => Self::Confirm,
        };
    }
}

impl TorrentSnapshot {
    fn is_stopped(&self) -> bool {
        self.state.to_ascii_lowercase().contains("paused")
    }
}

struct SourceSummary {
    kind: &'static str,
    value: String,
}

fn active_field_style(field: FormField) -> Style {
    match field {
        FormField::TorrentSource => Style::default().fg(Color::Yellow),
        FormField::DownloadDir => Style::default().fg(Color::Green),
    }
}

fn source_kind_style(summary: &SourceSummary) -> Style {
    match summary.kind {
        "remote URL" => Style::default().fg(Color::Cyan),
        "magnet link" => Style::default().fg(Color::Yellow),
        "local file" | "directory" | "local path" => Style::default().fg(Color::Green),
        "invalid" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    }
}

fn source_value_style(summary: &SourceSummary) -> Style {
    match summary.kind {
        "invalid" => Style::default().fg(Color::Red),
        "not set" => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
        _ => Style::default().fg(Color::White),
    }
}

fn output_value_style(raw_input: &str) -> Style {
    if raw_input.is_empty() {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    } else {
        Style::default().fg(Color::White)
    }
}

fn describe_torrent_source(input: &str) -> SourceSummary {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return SourceSummary {
            kind: "not set",
            value: "Waiting for a local file, remote URL, or magnet link.".to_string(),
        };
    }

    match TorrentSource::parse(trimmed) {
        Ok(TorrentSource::LocalFile(path)) => {
            let kind = if path.is_file() {
                "local file"
            } else if path.is_dir() {
                "directory"
            } else {
                "local path"
            };

            SourceSummary {
                kind,
                value: format_input_path(&path, path.is_dir()),
            }
        }
        Ok(TorrentSource::RemoteUrl(url)) => SourceSummary {
            kind: "remote URL",
            value: url,
        },
        Ok(TorrentSource::Magnet(link)) => SourceSummary {
            kind: "magnet link",
            value: link,
        },
        Err(error) => SourceSummary {
            kind: "invalid",
            value: format!("{error:#}"),
        },
    }
}

fn describe_output_directory(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return "Waiting for a directory path.".to_string();
    }

    match resolve_user_path(trimmed) {
        Ok(path) => format_input_path(&path, true),
        Err(error) => format!("{error:#}"),
    }
}

fn default_browser_root() -> Result<String> {
    match env::var("HOME") {
        Ok(_) => Ok("~/".to_string()),
        Err(_) => {
            let cwd =
                env::current_dir().context("failed to determine current working directory")?;
            Ok(format!("{}/", cwd.display()))
        }
    }
}

fn parent_input_path(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return default_browser_root();
    }

    let resolved = resolve_user_path(trimmed)?;
    let base = if trimmed.ends_with('/') {
        resolved
    } else {
        resolved.parent().map(Path::to_path_buf).unwrap_or(resolved)
    };

    let parent = base.parent().unwrap_or(base.as_path());
    Ok(format_input_path(parent, true))
}

fn format_input_path(path: &Path, is_dir: bool) -> String {
    let home = env::var("HOME").ok().map(std::path::PathBuf::from);

    let mut rendered = match home
        .as_ref()
        .and_then(|home_dir| path.strip_prefix(home_dir).ok())
    {
        Some(relative) if relative.as_os_str().is_empty() => "~".to_string(),
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    };

    if is_dir && !rendered.ends_with('/') {
        rendered.push('/');
    }

    rendered
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

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
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

fn inner_rect(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}
