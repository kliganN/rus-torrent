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
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    DefaultTerminal, Frame,
};
use std::time::Duration;

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
    modal: Option<ModalState>,
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
            total_downloads: 0,
            menu_state,
            downloads_state: ListState::default(),
            focus: FocusArea::Content,
            form_field: FormField::TorrentSource,
            torrent_source: String::new(),
            completion_state: None,
            downloads_view: DownloadsView::default(),
            modal: None,
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

        if self.modal.is_some() {
            return self.handle_modal_key(key);
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

    fn handle_modal_key(&mut self, key: KeyEvent) -> Result<()> {
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
                        self.apply_confirm_action(confirm.action);
                    } else {
                        self.status_line = format!("{} cancelled", confirm.action.title());
                    }
                }
                KeyCode::Char('y') => self.apply_confirm_action(confirm.action),
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
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_previous_download(),
            KeyCode::Down | KeyCode::Char('j') => self.select_next_download(),
            KeyCode::Home | KeyCode::Char('g') => self.select_first_download(),
            KeyCode::End | KeyCode::Char('G') => self.select_last_download(),
            KeyCode::Char('/') => self.open_filter_dialog(),
            KeyCode::Char('s') => self.open_sort_dialog(),
            KeyCode::Char('r') => self.reverse_sort_direction(),
            KeyCode::Char('m') => self.toggle_download_display_mode(),
            KeyCode::Char('c') => self.confirm_clear_filter(),
            KeyCode::Char('x') => self.open_confirm(ConfirmAction::ResetDownloadsView),
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

    fn select_previous_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        let current = self.downloads_state.selected().unwrap_or(0);
        self.downloads_state.select(Some(current.saturating_sub(1)));
    }

    fn select_next_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        let current = self.downloads_state.selected().unwrap_or(0);
        let next = (current + 1).min(self.downloads.len() - 1);
        self.downloads_state.select(Some(next));
    }

    fn select_first_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        self.downloads_state.select(Some(0));
    }

    fn select_last_download(&mut self) {
        if self.downloads.is_empty() {
            return;
        }

        self.downloads_state.select(Some(self.downloads.len() - 1));
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

    fn apply_confirm_action(&mut self, action: ConfirmAction) {
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
                Constraint::Length(5),
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
            .constraints([Constraint::Length(9), Constraint::Min(0)])
            .split(area);

        let controls = Paragraph::new(Text::from(vec![
            Line::from(format!("Active field: {}", self.form_field.title())),
            Line::from("Tab: next completion match for local paths"),
            Line::from("Shift+Tab: previous completion match"),
            Line::from("Up/Down: switch field"),
            Line::from("Empty field + Tab: browse from /"),
            Line::from("Paste: local path, HTTP/HTTPS .torrent URL, or magnet link"),
            Line::from("Enter: add torrent"),
            Line::from("F1: open help popup"),
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
                Line::from("Open 'Choose torrent source' and queue a source."),
                Line::from("F1 opens the full hotkey reference."),
            ])
        } else {
            Text::from(vec![
                Line::from("No downloads match the current filter."),
                Line::from(format!(
                    "Current filter: {}",
                    self.downloads_view.filter_summary()
                )),
                Line::from("Press / to edit the filter, or c to clear it."),
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
        let help = self.footer_help_text();

        let footer = Paragraph::new(Text::from(vec![
            Line::from(self.status_line.as_str()),
            Line::from(help),
        ]))
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
                "Choose Torrent Source",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("Up/Down: switch active field"),
            Line::from("Tab / Shift+Tab: cycle local path completion"),
            Line::from("Paste: insert file path, URL, or magnet link"),
            Line::from("Enter: queue the torrent"),
            Line::from(""),
            Line::styled(
                "Downloads",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from("Up/Down or j/k: select a torrent"),
            Line::from("Home/End or g/G: jump to first or last torrent"),
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
            Line::from("Enter: apply filter"),
            Line::from("Esc: cancel"),
            Line::from("Ctrl+U or Delete: clear the input"),
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
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(5),
            ])
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

        let help = Paragraph::new(Text::from(vec![
            Line::from("Up/Down or j/k: choose a sort field"),
            Line::from("Left/Right or r: toggle ascending / descending"),
            Line::from("Enter: apply"),
            Line::from("Esc: cancel"),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title("Controls").borders(Borders::ALL));
        frame.render_widget(help, chunks[2]);
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
            Span::raw("  "),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw(" apply  "),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw(" cancel"),
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

    fn footer_help_text(&self) -> &'static str {
        if let Some(modal) = &self.modal {
            return match modal {
                ModalState::Help => "F1/Esc/Enter close help | Ctrl+C/F10 force exit",
                ModalState::FilterInput { .. } => {
                    "Type terms | Enter apply filter | Ctrl+U/Delete clear | Esc cancel"
                }
                ModalState::SortPicker { .. } => {
                    "Up/Down choose field | Left/Right toggle order | Enter apply | Esc cancel"
                }
                ModalState::Confirm(_) => {
                    "Left/Right or Tab choose | Enter confirm | y/n quick answer | Esc cancel"
                }
            };
        }

        match self.current_screen() {
            Screen::AddTorrent => {
                "F1 help | Tab local completion | Up/Down switch field | Enter add torrent | Esc menu"
            }
            Screen::Downloads => {
                "F1 help | / filter | s sort | r reverse | m mode | c clear | x reset | q quit"
            }
            Screen::Server => "F1 help | Left/Right focus | Esc menu | q quit",
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
