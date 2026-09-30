use std::path::{Path, PathBuf};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
    },
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::core::CiteError;
use crate::core::db::{
    AllStats, DbManager, ProjectStats, StoredBuild, StoredDeployment, StoredPodcast, StoredTimeline,
};
use crate::core::project::{self, ProjectContext};
use crate::core::{compiler, deploy, doctor, scaffold};

const ACCENT: Color = Color::Cyan;
const WARN: Color = Color::Yellow;
const ERROR: Color = Color::Red;
const READY: Color = Color::Green;
const MUTED: Color = Color::DarkGray;
const ON_ACCENT: Color = Color::Black;

/// Oldest log lines are dropped past this cap so long sessions don't grow without bound.
const MAX_LOG_LINES: usize = 5_000;

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

pub struct Cmd {
    pub label: &'static str,
    pub desc: &'static str,
    pub args_hint: &'static str,
    pub needs_project: bool,
    pub id: CommandId,
}

#[derive(Clone, Copy, PartialEq)]
pub enum CommandId {
    Init,
    Build,
    Doctor,
    Deploy,
    Rollback,
}

pub const CMDS: &[Cmd] = &[
    Cmd {
        label: "init",
        desc: "Create a new project with starter files",
        args_hint: "<name>",
        needs_project: false,
        id: CommandId::Init,
    },
    Cmd {
        label: "build",
        desc: "Compile project into build/content.json",
        args_hint: "[--force]",
        needs_project: true,
        id: CommandId::Build,
    },
    Cmd {
        label: "doctor",
        desc: "Validate project, metadata, content, and assets",
        args_hint: "",
        needs_project: true,
        id: CommandId::Doctor,
    },
    Cmd {
        label: "deploy",
        desc: "Validate, build if needed, and publish episodes to Supabase",
        args_hint: "[--dry-run]",
        needs_project: true,
        id: CommandId::Deploy,
    },
    Cmd {
        label: "rollback",
        desc: "Remove the news items and uploads a deployment created",
        args_hint: "<deployment id>",
        needs_project: true,
        id: CommandId::Rollback,
    },
];

#[derive(Clone, Copy, PartialEq)]
pub enum TuiMode {
    Runner,
    CommandPalette,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Focus {
    Projects,
    Commands,
    Analytics,
    Logs,
}

/// Panels in `Tab` order.
const FOCUS_ORDER: [Focus; 4] = [
    Focus::Projects,
    Focus::Commands,
    Focus::Analytics,
    Focus::Logs,
];

#[derive(Clone, PartialEq)]
pub enum ProjectItemKind {
    LocalHeader,
    LocalProject(usize),
    ArchivedHeader,
    ArchivedProject(String),
}

#[derive(Clone)]
pub struct ProjectItem {
    pub kind: ProjectItemKind,
    pub label: String,
}

pub struct AnalyticsState {
    pub stats: Option<ProjectStats>,
    pub global: Option<AllStats>,
    pub podcasts: Vec<StoredPodcast>,
    pub timelines: Vec<StoredTimeline>,
    pub builds: Vec<StoredBuild>,
    pub deploys: Vec<StoredDeployment>,
    pub expanded: [bool; 4],
    pub scroll: usize,
}

pub struct CommandPaletteState {
    pub query: String,
    pub list_state: ListState,
}

struct EditorPick {
    files: Vec<PathBuf>,
    state: ListState,
    root: PathBuf,
}

pub struct AppState {
    cwd: PathBuf,
    pub roots: Vec<PathBuf>,
    pub db_projects: Vec<(String, String)>,
    pub project_items: Vec<ProjectItem>,
    pub projects_state: ListState,

    pub focus: Focus,
    pub cmds_state: ListState,

    pub log: Vec<String>,
    pub scroll: usize,
    pub busy: bool,
    pub arg_input: String,

    editor_pick: Option<EditorPick>,
    pending_edit: Option<PathBuf>,
    restore_prompt: Option<String>,
    pending_confirm: Option<(CommandId, String)>,

    rx: mpsc::Receiver<()>,
    tx: mpsc::Sender<()>,
    task: Option<JoinHandle<()>>,

    mode: TuiMode,
    analytics: AnalyticsState,
    command_palette: CommandPaletteState,

    local_expanded: bool,
    archived_expanded: bool,
    pending_init: bool,
}

impl AppState {
    pub async fn new(cwd: &Path) -> Self {
        let (tx, rx) = mpsc::channel(32);
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

        let mut cmds_state = ListState::default();
        cmds_state.select(Some(0));

        let mut state = Self {
            cwd: cwd.clone(),
            roots: Vec::new(),
            db_projects: Vec::new(),
            project_items: Vec::new(),
            projects_state: ListState::default(),
            focus: Focus::Commands,
            cmds_state,
            log: vec![],
            scroll: 0,
            busy: false,
            arg_input: String::new(),
            editor_pick: None,
            pending_edit: None,
            restore_prompt: None,
            pending_confirm: None,
            rx,
            tx,
            task: None,
            mode: TuiMode::Runner,
            analytics: AnalyticsState {
                stats: None,
                global: None,
                podcasts: Vec::new(),
                timelines: Vec::new(),
                builds: Vec::new(),
                deploys: Vec::new(),
                expanded: [true, true, true, true],
                scroll: 0,
            },
            command_palette: CommandPaletteState {
                query: String::new(),
                list_state: ListState::default(),
            },
            local_expanded: true,
            archived_expanded: true,
            pending_init: false,
        };

        state.refresh_projects().await;
        state.load_analytics_data().await;
        state
    }

    async fn load_db_projects() -> Vec<(String, String)> {
        if let Ok(db) = DbManager::open().await {
            db.list_db_projects().await.unwrap_or_default()
        } else {
            vec![]
        }
    }

    async fn refresh_projects(&mut self) {
        self.roots = project::discover_projects(&self.cwd);
        self.roots.sort();
        self.db_projects = Self::load_db_projects().await;
        self.rebuild_project_items();
    }

    fn rebuild_project_items(&mut self) {
        self.project_items.clear();

        self.project_items.push(ProjectItem {
            kind: ProjectItemKind::LocalHeader,
            label: if self.local_expanded {
                "▼ Local".into()
            } else {
                "▶ Local".into()
            },
        });

        if self.local_expanded {
            if self.roots.is_empty() {
                self.project_items.push(ProjectItem {
                    kind: ProjectItemKind::LocalProject(usize::MAX),
                    label: "  (none)".into(),
                });
            } else {
                for (i, root) in self.roots.iter().enumerate() {
                    let name = root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("?")
                        .to_string();
                    self.project_items.push(ProjectItem {
                        kind: ProjectItemKind::LocalProject(i),
                        label: format!("  {}", name),
                    });
                }
            }
        }

        self.project_items.push(ProjectItem {
            kind: ProjectItemKind::ArchivedHeader,
            label: if self.archived_expanded {
                "▼ Archived".into()
            } else {
                "▶ Archived".into()
            },
        });

        if self.archived_expanded {
            let archived = self.compute_archived();
            if archived.is_empty() {
                self.project_items.push(ProjectItem {
                    kind: ProjectItemKind::ArchivedProject("".into()),
                    label: "  (none)".into(),
                });
            } else {
                for name in archived {
                    self.project_items.push(ProjectItem {
                        kind: ProjectItemKind::ArchivedProject(name.clone()),
                        label: format!("  {name}"),
                    });
                }
            }
        }

        self.clamp_project_selection();
    }

    fn clamp_project_selection(&mut self) {
        if self.project_items.is_empty() {
            self.projects_state.select(None);
        } else {
            let last = self.project_items.len() - 1;
            match self.projects_state.selected() {
                Some(sel) => self.projects_state.select(Some(sel.min(last))),
                None => self.projects_state.select(Some(0)),
            }
        }
    }

    fn compute_archived(&self) -> Vec<String> {
        self.db_projects
            .iter()
            .filter(|(name, id)| {
                !self.roots.iter().any(|r| {
                    r.to_string_lossy().as_ref() == id.as_str()
                        || r.file_name().and_then(|n| n.to_str()) == Some(name.as_str())
                })
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    fn selected_root(&self) -> Option<PathBuf> {
        let sel = self.projects_state.selected()?;
        let item = self.project_items.get(sel)?;
        if let ProjectItemKind::LocalProject(i) = item.kind
            && i != usize::MAX
        {
            return self.roots.get(i).cloned();
        }
        None
    }

    fn filtered_commands(&self) -> Vec<usize> {
        let query = self.command_palette.query.to_lowercase();
        CMDS.iter()
            .enumerate()
            .filter(|(_, cmd)| {
                query.is_empty()
                    || cmd.label.to_lowercase().contains(&query)
                    || cmd.desc.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn clamp_palette_selection(&mut self) {
        let last = self.filtered_commands().len().saturating_sub(1);
        match self.command_palette.list_state.selected() {
            Some(sel) => self.command_palette.list_state.select(Some(sel.min(last))),
            None => self.command_palette.list_state.select(Some(0)),
        }
    }

    pub async fn load_analytics_data(&mut self) {
        let Ok(db) = DbManager::open().await else {
            return;
        };

        self.analytics.global = db.get_all_stats().await.ok();

        if let Some(root) = self.selected_root() {
            let project_id = root.to_string_lossy().to_string();
            self.analytics.stats = db.get_project_stats(&project_id).await.ok();
            self.analytics.podcasts = db
                .get_podcasts_with_content(&project_id)
                .await
                .ok()
                .unwrap_or_default();
            self.analytics.timelines = db.get_timelines(&project_id).await.ok().unwrap_or_default();
            self.analytics.builds = db
                .get_build_history(&project_id)
                .await
                .ok()
                .unwrap_or_default();
            self.analytics.deploys = db
                .get_deployment_history(&project_id)
                .await
                .ok()
                .unwrap_or_default();
        }

        self.analytics.scroll = 0;
    }

    pub async fn handle_key(&mut self, key: KeyEvent) {
        let ctrl_k = key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'));
        if ctrl_k {
            match self.mode {
                TuiMode::CommandPalette => self.close_palette(),
                TuiMode::Runner => self.mode = TuiMode::CommandPalette,
            }
            return;
        }

        match self.mode {
            TuiMode::Runner => self.handle_runner_key(key).await,
            TuiMode::CommandPalette => self.handle_command_palette_key(key),
        }
    }

    fn push_log(&mut self, line: String) {
        let was_at_bottom = self.scroll >= self.log.len().saturating_sub(1);
        if self.log.len() >= MAX_LOG_LINES {
            let overflow = self.log.len() + 1 - MAX_LOG_LINES;
            self.log.drain(..overflow);
            self.scroll = self.scroll.saturating_sub(overflow);
        }
        self.log.push(line);
        if was_at_bottom {
            self.scroll = self.log.len() - 1;
        }
    }

    fn close_palette(&mut self) {
        self.mode = TuiMode::Runner;
        self.command_palette.query.clear();
    }

    async fn handle_runner_key(&mut self, key: KeyEvent) {
        if self.restore_prompt.is_some() {
            self.handle_restore_key(key).await;
            return;
        }
        if self.pending_confirm.is_some() {
            self.handle_confirm_key(key);
            return;
        }
        if self.editor_pick.is_some() {
            self.handle_pick_key(key);
            return;
        }

        match key.code {
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if !self.busy {
                    self.refresh_projects().await;
                    self.load_analytics_data().await;
                    self.log.clear();
                    self.scroll = 0;
                    info!(">> Refreshed");
                }
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.focus == Focus::Projects && !self.busy {
                    self.open_edit_picker();
                }
            }
            KeyCode::Char('p' | 't' | 'b' | 'd')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if self.focus == Focus::Analytics && !self.busy {
                    let idx = match key.code {
                        KeyCode::Char('p') => 0,
                        KeyCode::Char('t') => 1,
                        KeyCode::Char('b') => 2,
                        _ => 3,
                    };
                    self.analytics.expanded[idx] = !self.analytics.expanded[idx];
                }
            }
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.focus == Focus::Projects && !self.busy {
                    self.local_expanded = !self.local_expanded;
                    self.rebuild_project_items();
                }
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.focus == Focus::Projects && !self.busy {
                    self.archived_expanded = !self.archived_expanded;
                    self.rebuild_project_items();
                }
            }
            _ => self.handle_runner_nav(key).await,
        }
    }

    async fn handle_runner_nav(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                if self.busy {
                    return;
                }
                let i = FOCUS_ORDER
                    .iter()
                    .position(|f| *f == self.focus)
                    .unwrap_or(0);
                let n = FOCUS_ORDER.len();
                self.focus = if key.code == KeyCode::Tab {
                    FOCUS_ORDER[(i + 1) % n]
                } else {
                    FOCUS_ORDER[(i + n - 1) % n]
                };
            }
            KeyCode::Up => match self.focus {
                Focus::Projects if !self.busy => self.projects_state.select_previous(),
                Focus::Analytics => {
                    self.analytics.scroll = self.analytics.scroll.saturating_sub(1);
                }
                Focus::Logs => self.scroll = self.scroll.saturating_sub(1),
                _ => {}
            },
            KeyCode::Down => match self.focus {
                Focus::Projects if !self.busy => self.projects_state.select_next(),
                Focus::Analytics => {
                    self.analytics.scroll = self.analytics.scroll.saturating_add(1);
                }
                Focus::Logs => self.scroll = self.scroll.saturating_add(1),
                _ => {}
            },
            KeyCode::Left => match self.focus {
                Focus::Commands if !self.busy => {
                    let sel = self.cmds_state.selected().unwrap_or(0);
                    if sel > 0 {
                        self.select_command(sel - 1);
                    }
                }
                _ => {}
            },
            KeyCode::Right => match self.focus {
                Focus::Commands if !self.busy => {
                    let sel = self.cmds_state.selected().unwrap_or(0);
                    if sel + 1 < CMDS.len() {
                        self.select_command(sel + 1);
                    }
                }
                _ => {}
            },
            KeyCode::Enter => {
                if !self.busy {
                    match self.focus {
                        Focus::Projects => {
                            let Some(sel) = self.projects_state.selected() else {
                                return;
                            };
                            let Some(item) = self.project_items.get(sel) else {
                                return;
                            };
                            match &item.kind {
                                ProjectItemKind::LocalHeader => {
                                    self.local_expanded = !self.local_expanded;
                                    self.rebuild_project_items();
                                }
                                ProjectItemKind::ArchivedHeader => {
                                    self.archived_expanded = !self.archived_expanded;
                                    self.rebuild_project_items();
                                }
                                ProjectItemKind::LocalProject(_) => {
                                    info!(">> Selected project");
                                    self.load_analytics_data().await;
                                }
                                ProjectItemKind::ArchivedProject(name) => {
                                    self.restore_prompt = Some(name.clone());
                                }
                            }
                        }
                        Focus::Commands => {
                            let cmd = &CMDS[self.cmds_state.selected().unwrap_or(0)];
                            if matches!(cmd.id, CommandId::Deploy | CommandId::Rollback)
                                && self.selected_root().is_some()
                            {
                                self.pending_confirm = Some((cmd.id, self.confirm_message(cmd.id)));
                            } else {
                                self.start_cmd();
                            }
                        }
                        _ => {}
                    }
                }
            }
            KeyCode::Esc if matches!(self.focus, Focus::Commands) && !self.arg_input.is_empty() => {
                self.arg_input.clear();
            }
            KeyCode::Backspace if !self.busy => {
                if matches!(self.focus, Focus::Commands)
                    && !CMDS[self.cmds_state.selected().unwrap_or(0)]
                        .args_hint
                        .is_empty()
                {
                    self.arg_input.pop();
                }
            }
            KeyCode::Char(ch)
                if !self.busy
                    && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT) =>
            {
                if matches!(self.focus, Focus::Commands)
                    && !CMDS[self.cmds_state.selected().unwrap_or(0)]
                        .args_hint
                        .is_empty()
                {
                    self.arg_input.push(ch);
                }
            }
            KeyCode::PageUp if matches!(self.focus, Focus::Analytics) => {
                self.analytics.scroll = self.analytics.scroll.saturating_sub(10)
            }
            KeyCode::PageDown if matches!(self.focus, Focus::Analytics) => {
                self.analytics.scroll = self.analytics.scroll.saturating_add(10)
            }
            _ => {}
        }
    }

    pub fn start_cmd(&mut self) {
        let root = self.selected_root();
        let sel = self.cmds_state.selected().unwrap_or(0);
        let cmd = &CMDS[sel];

        if cmd.needs_project && root.is_none() {
            error!("No projects found; select or init a project first");
            return;
        }

        let raw_args = std::mem::take(&mut self.arg_input);
        let arg_display = if raw_args.is_empty() {
            String::new()
        } else {
            format!(" ({})", raw_args)
        };
        info!(
            ">> {}{} {}",
            cmd.label,
            arg_display,
            root.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );

        self.busy = true;
        self.pending_init = cmd.id == CommandId::Init;
        let id = cmd.id;
        let tx = self.tx.clone();
        let cwd = self.cwd.clone();

        let handle = tokio::spawn(async move {
            match id {
                CommandId::Init => exec_init(cwd, raw_args).await,
                CommandId::Build => exec_build(root, raw_args).await,
                CommandId::Doctor => exec_doctor(root).await,
                CommandId::Deploy => exec_deploy(root, raw_args).await,
                CommandId::Rollback => exec_rollback(root, raw_args).await,
            }
            let _ = tx.send(()).await;
        });
        self.task = Some(handle);
    }

    fn select_command(&mut self, idx: usize) {
        self.cmds_state.select(Some(idx));
        self.arg_input.clear();
    }

    fn confirm_message(&self, id: CommandId) -> String {
        let target = self
            .selected_root()
            .and_then(|r| r.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        match id {
            CommandId::Deploy if self.arg_input.split_whitespace().any(|w| w == "--dry-run") => {
                format!("Preview deploying '{target}'?")
            }
            CommandId::Deploy => format!("Deploy '{target}' to Supabase?"),
            CommandId::Rollback => {
                let id = self.arg_input.trim();
                if id.is_empty() {
                    format!("Rollback '{target}'? (no deployment id set)")
                } else {
                    format!("Rollback '{target}' to '{id}'?")
                }
            }
            _ => String::new(),
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        if self.pending_confirm.is_none() {
            return;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                self.pending_confirm = None;
                self.start_cmd();
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.pending_confirm = None;
            }
            _ => {}
        }
    }

    fn handle_command_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_palette(),
            KeyCode::Up => {
                self.command_palette.list_state.select_previous();
                self.clamp_palette_selection();
            }
            KeyCode::Down => {
                let len = self.filtered_commands().len();
                let sel = self.command_palette.list_state.selected().unwrap_or(0);
                if sel + 1 < len {
                    self.command_palette.list_state.select(Some(sel + 1));
                }
            }
            KeyCode::Enter => {
                let filtered = self.filtered_commands();
                if let Some(&cmd_idx) = self
                    .command_palette
                    .list_state
                    .selected()
                    .and_then(|i| filtered.get(i))
                {
                    self.select_command(cmd_idx);
                    self.close_palette();
                    self.focus = Focus::Commands;
                    let cmd = &CMDS[cmd_idx];
                    if matches!(cmd.id, CommandId::Deploy | CommandId::Rollback)
                        && self.selected_root().is_some()
                    {
                        self.pending_confirm = Some((cmd.id, self.confirm_message(cmd.id)));
                    } else {
                        self.start_cmd();
                    }
                }
            }
            KeyCode::Backspace => {
                self.command_palette.query.pop();
                self.clamp_palette_selection();
            }
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.command_palette.query.push(c);
                self.clamp_palette_selection();
            }
            _ => {}
        }
    }

    fn handle_pick_key(&mut self, key: KeyEvent) {
        let Some(pick) = self.editor_pick.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Up => pick.state.select_previous(),
            KeyCode::Down => pick.state.select_next(),
            KeyCode::Enter => {
                if let Some(idx) = pick.state.selected() {
                    self.pending_edit = pick.files.get(idx).cloned();
                    self.editor_pick = None;
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => self.editor_pick = None,
            _ => {}
        }
    }

    async fn handle_restore_key(&mut self, key: KeyEvent) {
        let Some(name) = self.restore_prompt.clone() else {
            return;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let target = self.cwd.join(&name);
                match restore_archived(&target, &name, &self.db_projects).await {
                    Ok(summary) => {
                        info!(
                            "Restored project {name} at {} ({} podcast(s), {} timeline event(s))",
                            target.display(),
                            summary.podcasts,
                            summary.timeline_events
                        );
                        for warning in summary.warnings {
                            warn!("{warning}");
                        }
                    }
                    Err(e) => {
                        warn!("No local snapshot to restore ({e}); scaffolding fresh project");
                        if let Err(e) = scaffold::init_project(&name, &target) {
                            error!("Failed to restore project: {e}");
                        } else {
                            info!("Created fresh project {name} at {}", target.display());
                        }
                    }
                }
                self.restore_prompt = None;
                self.refresh_projects().await;
                self.load_analytics_data().await;
                if let Some(idx) = self
                    .project_items
                    .iter()
                    .position(|item| matches!(&item.kind, ProjectItemKind::LocalProject(_)))
                {
                    self.projects_state.select(Some(idx));
                    self.open_edit_picker();
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Char('q') => {
                self.restore_prompt = None;
            }
            _ => {}
        }
    }

    fn open_edit_picker(&mut self) {
        let Some(root) = self.selected_root() else {
            error!("No project selected");
            return;
        };
        let mut files = Vec::new();
        collect_files(&root, &mut files);
        files.sort();
        let mut state = ListState::default();
        state.select(Some(0));
        self.editor_pick = Some(EditorPick { files, state, root });
    }
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str())
                    && (name == "build" || name.starts_with('.'))
                {
                    continue;
                }
                collect_files(&path, files);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
}

async fn restore_archived(
    target: &Path,
    name: &str,
    db_projects: &[(String, String)],
) -> Result<scaffold::RestoreSummary, CiteError> {
    let (_, project_id) = db_projects
        .iter()
        .find(|(n, _)| n == name)
        .ok_or_else(|| CiteError::Config(format!("No local record for '{name}'")))?;
    let db = DbManager::open().await?;
    scaffold::restore_project(&db, project_id, target).await
}

fn block(title: impl Into<String>, focused: bool) -> Block<'static> {
    let border_style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new()
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(border_style)
        .title(title.into())
}

fn color_log_line(l: &str) -> Line<'static> {
    if l.contains("ERROR") {
        Line::styled(l.to_string(), Style::new().fg(ERROR).bold())
    } else if l.contains("WARN") {
        Line::styled(l.to_string(), Style::new().fg(WARN))
    } else {
        Line::from(l.to_string())
    }
}

pub async fn run_tui(
    mut log_rx: mpsc::UnboundedReceiver<String>,
    cli_root: PathBuf,
) -> Result<(), CiteError> {
    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;
    terminal
        .clear()
        .map_err(|e| CiteError::Config(format!("{e}")))?;

    let mut app = AppState::new(&cli_root).await;
    let mut events = EventStream::new();

    loop {
        terminal
            .draw(|f| render(f, &mut app))
            .map_err(|e| CiteError::Config(format!("{e}")))?;

        tokio::select! {
            biased;
            Some(()) = app.rx.recv() => {
                app.busy = false;
                app.task = None;
                info!(">> Command complete, refreshing");
                app.refresh_projects().await;
                app.load_analytics_data().await;
                if app.pending_init {
                    app.pending_init = false;
                    if !app.project_items.is_empty() {
                        app.projects_state.select(Some(0));
                        app.focus = Focus::Projects;
                    }
                }
            }
            Some(line) = log_rx.recv() => app.push_log(line),
            Some(Ok(event)) = events.next() => {
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                {
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    let quit = (key.code == KeyCode::Char('q')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                        || (ctrl_c && !app.busy)
                        || (key.code == KeyCode::Esc
                            && app.mode == TuiMode::Runner
                            && app.restore_prompt.is_none()
                            && app.pending_confirm.is_none()
                            && app.editor_pick.is_none()
                            && !(app.focus == Focus::Commands && !app.arg_input.is_empty()));

                    if quit {
                        break;
                    } else if ctrl_c && app.busy {
                        if let Some(task) = app.task.take() {
                            task.abort();
                        }
                        app.busy = false;
                        app.pending_init = false;
                        info!(">> Command cancelled");
                    } else {
                        app.handle_key(key).await;

                        if let Some(path) = app.pending_edit.take() {
                            edit_file(&mut terminal, &path)
                                .await
                                .map_err(|e| CiteError::Config(format!("{e}")))?;
                        }
                    }
                }
            }
        }
    }

    if let Some(task) = app.task.take() {
        task.abort();
    }
    Ok(())
}

fn render(frame: &mut Frame, app: &mut AppState) {
    let [header, body, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_header(frame, header, app);
    render_body(frame, body, app);
    render_statusbar(frame, status, app);

    if app.restore_prompt.is_some() {
        render_restore_prompt(frame, frame.area(), app);
    } else if app.pending_confirm.is_some() {
        render_pending_confirm(frame, frame.area(), app);
    } else if app.editor_pick.is_some() {
        render_editor_pick(frame, frame.area(), app);
    }

    if app.mode == TuiMode::CommandPalette {
        render_command_palette(frame, frame.area(), app);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &AppState) {
    let (left_text, style) = if app.busy {
        let cmd = &CMDS[app.cmds_state.selected().unwrap_or(0)];
        (
            format!(" Executing: {} on {} ", cmd.label, app.cwd.display()),
            Style::new()
                .fg(ON_ACCENT)
                .bg(WARN)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (" Ready ".to_string(), Style::new().fg(ON_ACCENT).bg(READY))
    };
    let version = Span::styled(
        format!("v{}", env!("CARGO_PKG_VERSION")),
        Style::new().dim(),
    );

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(left_text, style))),
        left_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(version)).alignment(Alignment::Right),
        right_area,
    );
}

fn render_body(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let [left, middle, right] = Layout::horizontal([
        Constraint::Max(20),
        Constraint::Fill(2),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [cmd_area, logs_area] =
        Layout::vertical([Constraint::Max(8), Constraint::Min(3)]).areas(middle);

    render_categorized_project_list(frame, left, app);
    render_commands_pane(frame, cmd_area, app);
    render_log(frame, logs_area, app);
    render_analytics_content(frame, right, app);
}

fn render_categorized_project_list(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let is_focused = matches!(app.focus, Focus::Projects);

    let items: Vec<ListItem> = app
        .project_items
        .iter()
        .map(|item| {
            let style = match item.kind {
                ProjectItemKind::LocalHeader | ProjectItemKind::ArchivedHeader => {
                    if is_focused {
                        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().add_modifier(Modifier::BOLD)
                    }
                }
                _ => Style::new(),
            };
            ListItem::new(Line::from(Span::styled(item.label.clone(), style)))
        })
        .collect();

    let block_widget = block(" Projects ", is_focused);
    let inner_area = block_widget.inner(area);
    frame.render_widget(block_widget, area);

    let list = List::new(items).highlight_style(
        Style::new()
            .fg(ON_ACCENT)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD),
    );
    frame.render_stateful_widget(list, inner_area, &mut app.projects_state);

    let total = app.project_items.len();
    let visible = inner_area.height as usize;
    if total > visible {
        let selected = app.projects_state.selected().unwrap_or(0);
        let max_scroll = total.saturating_sub(visible);
        let scroll_pos = selected.min(max_scroll);
        let mut scroll_state = ScrollbarState::default()
            .content_length(total)
            .position(scroll_pos);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut scroll_state,
        );
    }
}

fn render_commands_pane(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let is_focused = matches!(app.focus, Focus::Commands);
    let block_widget = block(" Commands ", is_focused);
    let inner = block_widget.inner(area);
    frame.render_widget(block_widget, area);

    let [tabs_area, doc_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);

    let tab_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(if is_focused {
            Style::new().fg(ACCENT)
        } else {
            Style::default()
        });
    let tab_inner = tab_block.inner(tabs_area);
    frame.render_widget(tab_block, tabs_area);

    let titles: Vec<Line> = CMDS.iter().map(|cmd| Line::from(cmd.label)).collect();
    let tabs = Tabs::new(titles)
        .select(app.cmds_state.selected().unwrap_or(0))
        .divider(symbols::DOT)
        .highlight_style(Style::new().bold().fg(ACCENT));
    frame.render_widget(tabs, tab_inner);

    let sel = app.cmds_state.selected().unwrap_or(0);
    let cmd = &CMDS[sel];

    let mut lines = vec![Line::from(vec![Span::raw(format!(
        "{}: {}",
        cmd.label, cmd.desc
    ))])];

    if !cmd.args_hint.is_empty() {
        lines.push(Line::from(format!("Arguments: {}", cmd.args_hint)));
        let input_text = if app.arg_input.is_empty() {
            "Awaiting input...".to_string()
        } else {
            app.arg_input.clone()
        };
        let cursor_style = if is_focused {
            Style::new().fg(WARN).add_modifier(Modifier::UNDERLINED)
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::raw("Input: "),
            Span::styled(input_text, cursor_style),
        ]));
    }

    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        doc_area,
    );
}

fn render_log(frame: &mut Frame, area: Rect, app: &AppState) {
    let is_focused = matches!(app.focus, Focus::Logs);
    let visible_lines = area.height.saturating_sub(2) as usize;
    let max_scroll = app.log.len().saturating_sub(visible_lines);
    let scroll_y = app.scroll.min(max_scroll);
    let end = (scroll_y + visible_lines).min(app.log.len());

    let lines: Vec<Line> = app
        .log
        .get(scroll_y..end)
        .unwrap_or(&[])
        .iter()
        .map(|l| color_log_line(l))
        .collect();
    let block_widget = block(" Logs ", is_focused);
    let inner_area = block_widget.inner(area);

    frame.render_widget(block_widget, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
        inner_area,
    );

    if app.log.len() > visible_lines {
        let mut scroll_state = ScrollbarState::default()
            .content_length(app.log.len())
            .position(scroll_y);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut scroll_state,
        );
    }
}

fn pane_label(focus: Focus) -> &'static str {
    match focus {
        Focus::Projects => " Projects ",
        Focus::Commands => " Commands ",
        Focus::Analytics => " Analytics ",
        Focus::Logs => " Logs ",
    }
}

fn render_statusbar(frame: &mut Frame, area: Rect, app: &AppState) {
    let mode_label = match app.mode {
        TuiMode::Runner => pane_label(app.focus),
        TuiMode::CommandPalette => " Command Palette ",
    };

    let help_text = if app.busy {
        "[Ctrl+C] cancel  [Ctrl+Q]"
    } else if app.restore_prompt.is_some() || app.pending_confirm.is_some() {
        "[Y]es  [N]o  [Esc] cancel  [Ctrl+Q]"
    } else {
        match app.mode {
            TuiMode::CommandPalette => "[type to filter] [↑/↓] [Enter] [Esc] [Ctrl+Q]",
            TuiMode::Runner => match app.focus {
                Focus::Projects => "[↑/↓] [Ctrl+R] [Enter] [Ctrl+E/L/A] [Ctrl+K] [Ctrl+Q]",
                Focus::Commands => {
                    let has_args = !CMDS[app.cmds_state.selected().unwrap_or(0)]
                        .args_hint
                        .is_empty();
                    if has_args {
                        "[←/→] [Enter] [type args] [Ctrl+K] [Ctrl+Q]"
                    } else {
                        "[←/→] [Enter] [Ctrl+K] [Ctrl+Q]"
                    }
                }
                Focus::Analytics => "[↑/↓] [Ctrl+R] [Enter] [Ctrl+P/T/B/D] [Ctrl+K] [Ctrl+Q]",
                Focus::Logs => "[↑/↓] [Ctrl+K] [Ctrl+Q]",
            },
        }
    };

    let [left_area, right_area] =
        Layout::horizontal([Constraint::Length(18), Constraint::Fill(1)]).areas(area);

    let left_style = Style::new().bold().bg(ACCENT).fg(ON_ACCENT);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(mode_label, left_style))),
        left_area,
    );

    frame.render_widget(
        Paragraph::new(Line::from(help_text)).alignment(Alignment::Right),
        right_area,
    );
}

fn render_command_palette(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let width = 70u16.min(area.width.saturating_sub(4));
    let height = 15u16.min(area.height.saturating_sub(4));
    let [_, mid, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .areas(mid);

    frame.render_widget(Clear, popup);
    let filtered = app.filtered_commands();

    let items: Vec<ListItem> = filtered
        .iter()
        .map(|&cmd_idx| {
            let cmd = &CMDS[cmd_idx];
            ListItem::new(format!(" {}: {}", cmd.label, cmd.desc))
        })
        .collect();

    let palette_block = Block::default()
        .borders(Borders::ALL)
        .title(" Command Palette ")
        .border_style(Style::new().fg(WARN));

    let inner = palette_block.inner(popup);
    frame.render_widget(palette_block, popup);

    let [input_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(
            app.command_palette.query.clone(),
        )])),
        input_area,
    );

    if filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("No matching commands", Style::new().fg(MUTED)))
                .alignment(Alignment::Center),
            list_area,
        );
        return;
    }

    let list = List::new(items)
        .highlight_style(Style::new().bold())
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, list_area, &mut app.command_palette.list_state);
}

fn render_confirm_popup(frame: &mut Frame, area: Rect, title: &str, message: &str) {
    let width = 50u16.min(area.width.saturating_sub(2));
    let height = 6u16;
    let [_, mid, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .areas(mid);

    let text = Text::from(vec![
        Line::from(message.to_string()),
        Line::from(""),
        Line::from(Span::styled("[Y]es  [N]o", Style::new().bold())),
    ]);
    let confirm_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(Style::new().fg(WARN));
    let p = Paragraph::new(text)
        .block(confirm_block)
        .alignment(Alignment::Center);
    frame.render_widget(Clear, popup);
    frame.render_widget(p, popup);
}

fn render_restore_prompt(frame: &mut Frame, area: Rect, app: &AppState) {
    let Some(ref name) = app.restore_prompt else {
        return;
    };
    render_confirm_popup(
        frame,
        area,
        "Restore",
        &format!("Restore project \"{name}\" locally?"),
    );
}

fn render_pending_confirm(frame: &mut Frame, area: Rect, app: &AppState) {
    let Some((_, ref message)) = app.pending_confirm else {
        return;
    };
    render_confirm_popup(frame, area, "Confirm", message);
}

fn render_editor_pick(frame: &mut Frame, area: Rect, app: &mut AppState) {
    let Some(pick) = &mut app.editor_pick else {
        return;
    };
    let root = &pick.root;
    let width = 60u16.min(area.width.saturating_sub(2));
    let height = (pick.files.len() as u16 + 4).min(area.height.saturating_sub(2));
    let [_, mid, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(width),
        Constraint::Fill(1),
    ])
    .areas(mid);

    let items: Vec<ListItem> = pick
        .files
        .iter()
        .map(|f| ListItem::new(f.strip_prefix(root).unwrap_or(f).to_string_lossy()))
        .collect();
    let edit_block = Block::default()
        .borders(Borders::ALL)
        .title(" Select File to Edit ")
        .border_style(Style::new().fg(WARN));

    let list = List::new(items)
        .block(edit_block)
        .highlight_style(Style::new().bold())
        .highlight_symbol("▸ ");

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut pick.state);
}

fn render_analytics_content(frame: &mut Frame, area: Rect, app: &AppState) {
    let mut lines: Vec<Line> = Vec::new();
    render_analytics_global(&mut lines, &app.analytics);
    lines.push(Line::from(""));
    render_analytics_project_stats(&mut lines, &app.analytics);

    let a = &app.analytics;
    let sections = [
        ("Podcasts Metadata", podcast_rows(&a.podcasts)),
        ("Timelines", timeline_rows(&a.timelines)),
        ("Build History", build_rows(&a.builds)),
        ("Deployment History", deploy_rows(&a.deploys)),
    ];
    for (i, (title, rows)) in sections.into_iter().enumerate() {
        lines.push(Line::from(""));
        push_section(&mut lines, title, a.expanded[i], rows);
    }

    let is_focused = matches!(app.focus, Focus::Analytics);
    let block_widget = block(" Analytics ", is_focused);
    let inner_area = block_widget.inner(area);
    frame.render_widget(block_widget, area);

    let visible_lines = inner_area.height as usize;
    let total_lines = lines.len();
    let max_scroll = total_lines.saturating_sub(visible_lines);
    let scroll_y = app.analytics.scroll.min(max_scroll);

    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll_y as u16, 0)),
        inner_area,
    );

    if total_lines > visible_lines {
        let mut scroll_state = ScrollbarState::default()
            .content_length(total_lines)
            .position(scroll_y);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            area,
            &mut scroll_state,
        );
    }
}

fn push_section(lines: &mut Vec<Line>, title: &str, expanded: bool, rows: Vec<String>) {
    lines.push(Line::from(Span::styled(
        title.to_string(),
        Style::new().bold(),
    )));
    lines.push(Line::from(if expanded { "▼" } else { "▶" }));
    if expanded {
        for row in rows {
            lines.push(Line::from(format!("  {row}")));
        }
    }
}

fn podcast_rows(podcasts: &[StoredPodcast]) -> Vec<String> {
    podcasts
        .iter()
        .map(|p| {
            let tag = if p.category.is_empty() {
                String::new()
            } else {
                format!(" [{}]", p.category)
            };
            let audio_flag = if p.has_audio { " [A]" } else { "" };
            let thumb_flag = if p.has_thumbnail { " [T]" } else { "" };
            let file_name = p.file.rsplit('/').next().unwrap_or(&p.file);
            format!(
                "{}{}{}{}  ({}w) <{}>",
                p.title, tag, audio_flag, thumb_flag, p.word_count, file_name
            )
        })
        .collect()
}

fn timeline_rows(timelines: &[StoredTimeline]) -> Vec<String> {
    timelines
        .iter()
        .map(|t| {
            let extra = t
                .entry_type
                .as_ref()
                .map(|et| format!(" ({et})"))
                .unwrap_or_default();
            let linked = t.url.is_some();
            format!(
                "{}  {}{}{}",
                t.date.as_deref().unwrap_or("??"),
                t.title,
                extra,
                if linked { " ↗" } else { "" }
            )
        })
        .collect()
}

fn build_rows(builds: &[StoredBuild]) -> Vec<String> {
    builds
        .iter()
        .map(|b| {
            let date = b.built_at.get(..16).unwrap_or(&b.built_at);
            let flag = if b.was_incremental {
                "(incr)"
            } else {
                "(full)"
            };
            format!(
                "{} p:{} w:{} {:>4}ms {}",
                date, b.podcast_count, b.total_words, b.duration_ms, flag
            )
        })
        .collect()
}

fn deploy_rows(deploys: &[StoredDeployment]) -> Vec<String> {
    deploys
        .iter()
        .map(|d| {
            let status = if d.success { "ok" } else { "fail" };
            format!(
                "{}  {}  {}  n:{} a:{}",
                d.deployment_id, d.deployed_at, status, d.news_count, d.asset_count
            )
        })
        .collect()
}

fn render_analytics_global(lines: &mut Vec<Line>, analytics: &AnalyticsState) {
    lines.push(Line::from(Span::styled(
        "Global Summary",
        Style::new().bold(),
    )));
    if let Some(ref global) = analytics.global {
        lines.push(Line::from(format!(
            "  Projects   : {}",
            global.project_count
        )));
        lines.push(Line::from(format!(
            "  Podcasts   : {}",
            global.total_podcasts
        )));
        lines.push(Line::from(format!(
            "  Timelines  : {}",
            global.total_timelines
        )));
        lines.push(Line::from(format!("  Words      : {}", global.total_words)));
        lines.push(Line::from(format!(
            "  Builds     : {}",
            global.total_builds
        )));
    } else {
        lines.push(Line::from("  No data"));
    }
}

fn render_analytics_project_stats(lines: &mut Vec<Line>, analytics: &AnalyticsState) {
    lines.push(Line::from(Span::styled(
        "Project Statistics",
        Style::new().bold(),
    )));
    if let Some(ref stats) = analytics.stats {
        lines.push(Line::from(format!(
            "  Podcasts     : {}",
            stats.podcast_count
        )));
        lines.push(Line::from(format!(
            "  Total Words  : {}",
            stats.total_words
        )));
        lines.push(Line::from(format!(
            "  Timelines    : {}",
            stats.timeline_count
        )));
        lines.push(Line::from(format!(
            "  Builds       : {}",
            stats.build_count
        )));
        lines.push(Line::from(format!(
            "  Deployments  : {}",
            stats.deployment_count
        )));
        if let Some(ref last) = stats.last_built {
            lines.push(Line::from(format!(
                "  Last Build   : {}",
                last.get(..19).unwrap_or(last)
            )));
        }
        if let Some(ref last) = stats.last_deployed {
            lines.push(Line::from(format!(
                "  Last Deploy  : {}",
                last.get(..19).unwrap_or(last)
            )));
        }
    } else {
        lines.push(Line::from("  No Project Selected"));
    }
}

async fn edit_file(terminal: &mut ratatui::DefaultTerminal, path: &Path) -> std::io::Result<()> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    info!(">> Editing {} in {editor}", path.display());
    let before = file_digest(path).await;

    ratatui::restore();
    let editor_clone = editor.clone();
    let path_clone = path.to_path_buf();

    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&editor_clone)
            .arg(&path_clone)
            .status()
    })
    .await
    .map_err(std::io::Error::other)??;

    *terminal = ratatui::init();
    terminal.clear()?;

    match status {
        s if s.success() => {
            if file_digest(path).await != before {
                info!("Edited {}", path.display());
            } else {
                info!("No changes to {}", path.display());
            }
        }
        s => warn!("Editor exited with {s}"),
    }
    Ok(())
}

async fn file_digest(path: &Path) -> Option<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || crate::core::cache::sha256_file(&path).ok())
        .await
        .ok()
        .flatten()
}

async fn load_project_context(root: Option<PathBuf>) -> Option<(ProjectContext, DbManager)> {
    let root = root?;
    let ctx = match ProjectContext::load(&root) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to load project: {e}");
            return None;
        }
    };
    let db = match DbManager::open().await {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to open database: {e}");
            return None;
        }
    };
    Some((ctx, db))
}

async fn exec_init(cwd: PathBuf, raw: String) {
    let name = raw.split_whitespace().next().unwrap_or("new-project");
    let target = cwd.join(name);
    match scaffold::init_project(name, &target) {
        Ok(_) => info!("Project '{name}' created at {}", target.display()),
        Err(e) => error!("Init failed: {e}"),
    }
}

async fn exec_build(root: Option<PathBuf>, raw: String) {
    let Some((ctx, db)) = load_project_context(root).await else {
        return;
    };
    let force = raw.split_whitespace().any(|w| w == "--force");
    match compiler::compile(&db, &ctx, force).await {
        Ok(outcome) => outcome.emit(),
        Err(e) => error!("Build failed: {e}"),
    }
}

async fn exec_doctor(root: Option<PathBuf>) {
    let Some((ctx, db)) = load_project_context(root).await else {
        return;
    };
    let outcome = doctor::run(&db, &ctx).await;
    outcome.emit();
    if !outcome.has_errors() && !outcome.has_warnings() {
        info!("Doctor check complete; no issues found");
    }
}

async fn exec_deploy(root: Option<PathBuf>, raw: String) {
    let Some((ctx, db)) = load_project_context(root).await else {
        return;
    };
    let dry_run = raw.split_whitespace().any(|w| w == "--dry-run");
    match deploy::deploy(&db, &ctx, dry_run).await {
        Ok(msg) => info!("{msg}"),
        Err(e) => error!("Deploy failed: {e}"),
    }
}

async fn exec_rollback(root: Option<PathBuf>, raw: String) {
    let id = raw.split_whitespace().next().unwrap_or("");
    if id.is_empty() {
        error!("No deployment ID provided");
        return;
    }
    let Some(root) = root else {
        error!("No project selected");
        return;
    };
    let ctx = match ProjectContext::load(&root) {
        Ok(ctx) => ctx,
        Err(e) => return error!("Failed to load project: {e}"),
    };
    match deploy::rollback(&ctx, id).await {
        Ok(msg) => info!("{msg}"),
        Err(e) => error!("Rollback failed: {e}"),
    }
}
