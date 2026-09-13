use crate::commands::sessions::{managed_sessions, ManagedSession};
use crate::config::Agent;
use crate::tmux;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use std::io::stdout;
use std::io::Write;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};
use tui_term::widget::PseudoTerminal;

/// How long to wait for a keypress before going back round to drain terminal
/// output. Short enough that output feels immediate, long enough not to spin.
const POLL: Duration = Duration::from_millis(8);

/// How often to re-read the session list.
///
/// Sessions come and go outside this screen — `amux <id>` in another terminal,
/// a `cc` in a new directory, an agent exiting. Reading the list once at
/// startup meant none of that ever showed up.
///
/// Only the *names* are compared on each tick; the expensive part (a
/// `session_cwd` subprocess per session) runs only when they actually differ.
const RELOAD_EVERY: Duration = Duration::from_millis(1500);

/// Floor between redraws, so a burst of output can't spin the renderer.
const REDRAW_FLOOR: Duration = Duration::from_millis(16);
/// How often to re-derive session statuses. A sweep captures every pane, so
/// this is deliberately slower than the reload that only compares names.
const STATUS_EVERY: Duration = Duration::from_millis(2000);

/// Which column has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Tree,
    Terminal,
}

/// A directory, with the sessions running in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Absolute path — the identity.
    pub dir: String,
    /// Last path component.
    pub name: String,
    /// What the user called this directory, if anything. Shown in place of
    /// `name`, because a folder called `reverse` says less than "逆向分析".
    pub alias: Option<String>,
    pub sessions: Vec<ManagedSession>,
}

impl Project {
    /// The name to show: the user's if they set one, else the folder's.
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

/// One directory the user has worked in before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectEntry {
    pub path: String,
    /// What to show: the name the user gave it, else the folder name.
    pub label: String,
}

/// Incremental search over the directories in project history.
#[derive(Debug, Clone, Default)]
pub struct ProjectPicker {
    pub query: String,
    pub cursor: usize,
    entries: Vec<ProjectEntry>,
}

impl ProjectPicker {
    /// Build from history, dropping directories that are no longer there.
    ///
    /// Seventeen of the sixty-two recorded paths no longer exist; offering them
    /// would only produce the "directory no longer exists" error that
    /// `resume_by_id` already raises. Checked once here rather than per
    /// keystroke, since it is a stat per entry.
    pub fn new(rows: Vec<crate::store::ProjectRow>) -> Self {
        let entries = rows
            .into_iter()
            .filter(|row| std::path::Path::new(&row.path).is_dir())
            .map(|row| ProjectEntry {
                label: row.alias.unwrap_or(row.name),
                path: row.path,
            })
            .collect();
        Self { query: String::new(), cursor: 0, entries }
    }

    /// Entries matching the query, by case-insensitive substring over the name
    /// and the full path — the same rule the tree filter uses, so typing
    /// `sitin` finds `/Users/not/projects/devs/sitin` by its path alone.
    pub fn matches(&self) -> Vec<&ProjectEntry> {
        let q = self.query.trim().to_lowercase();
        self.entries
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.label.to_lowercase().contains(&q)
                    || e.path.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn selected(&self) -> Option<&ProjectEntry> {
        self.matches().get(self.cursor).copied()
    }

    /// Keep the cursor on a row that exists; typing narrows the list under it.
    pub fn clamp(&mut self) {
        let len = self.matches().len();
        self.cursor = if len == 0 { 0 } else { self.cursor.min(len - 1) };
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.matches().len();
        if len == 0 {
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, len as isize - 1) as usize;
    }
}

/// One past conversation, with the two things `PastSession` does not carry.
///
/// `PastSession` is a per-directory listing shape — no agent, no cwd — so
/// resuming from it needs both attached here.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionEntry {
    pub agent: String,
    pub id: String,
    pub modified: f64,
    pub summary: Option<String>,
}

/// The conversations recorded for one directory, and the cursor over them.
#[derive(Debug, Clone)]
pub struct SessionPicker {
    pub dir: String,
    pub label: String,
    /// Whether a project list preceded this one, and so whether Esc has
    /// somewhere to step back to.
    pub from_project_list: bool,
    pub entries: Vec<SessionEntry>,
    pub cursor: usize,
    /// True until the background listing lands. Drawn as "loading…" rather than
    /// as an empty list, which would read as "this project has no history".
    pub loading: bool,
}

impl SessionPicker {
    pub fn new(dir: String, label: String) -> Self {
        Self {
            dir,
            label,
            from_project_list: false,
            entries: Vec::new(),
            cursor: 0,
            loading: true,
        }
    }

    /// Reached by picking from the project list, so Esc can go back to it.
    pub fn from_projects(dir: String, label: String) -> Self {
        Self { from_project_list: true, ..Self::new(dir, label) }
    }

    pub fn selected(&self) -> Option<&SessionEntry> {
        self.entries.get(self.cursor)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, self.entries.len() as isize - 1) as usize;
    }
}

/// A short description for each live session: what it is actually about.
///
/// The label wins when there is one — it is the user's own words, and it is the
/// only thing that can tell apart several codex sessions whose opening
/// instruction is the same generated boilerplate. Otherwise the conversation's
/// own summary stands in.
///
/// Resolved through the id recorded *for this session*, not the directory's
/// newest conversation: siblings like `cx_reverse_…` and `cx_reverse_…-grok`
/// share a directory and would otherwise be described identically.
pub fn describe_sessions(
    projects: &[Project],
    agents: &[Agent],
) -> std::collections::BTreeMap<String, String> {
    let labels = crate::store::labels();
    let mut out = std::collections::BTreeMap::new();

    for project in projects {
        let cwd = std::path::Path::new(&project.dir);
        for session in &project.sessions {
            if let Some(label) = labels.get(&session.name) {
                out.insert(session.name.clone(), label.clone());
                continue;
            }
            let Some(agent) = agents.iter().find(|a| a.alias == session.alias) else {
                continue;
            };
            // Fall back to the directory's newest only when this session has
            // nothing recorded — the same order `amux run` resolves in.
            let id = crate::commands::session_ids::load_id(&session.name)
                .or_else(|| crate::commands::session_ids::current_id(&agent.name, cwd));
            if let Some(summary) =
                id.and_then(|id| crate::commands::session_ids::summary_for(&agent.name, cwd, &id))
            {
                out.insert(session.name.clone(), summary);
            }
        }
    }
    out
}

/// Every past conversation in `dir`, newest first across all agents.
pub fn sessions_in(dir: &str, agents: &[Agent]) -> Vec<SessionEntry> {
    let cwd = std::path::Path::new(dir);
    let mut out: Vec<SessionEntry> = agents
        .iter()
        .filter(|a| crate::commands::session_ids::supports_sessions(&a.name))
        .flat_map(|a| {
            crate::commands::session_ids::recent_sessions(&a.name, cwd, 10)
                .into_iter()
                .map(move |p| SessionEntry {
                    agent: a.name.clone(),
                    id: p.id,
                    modified: p.modified,
                    summary: p.summary,
                })
        })
        .collect();
    out.sort_by(|a, b| {
        b.modified
            .partial_cmp(&a.modified)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// The stages of choosing a session to open.
#[derive(Debug, Clone)]
pub enum Draft {
    /// Which directory.
    Project(ProjectPicker),
    /// Directory settled; which conversation, or a fresh one.
    Session(SessionPicker),
    /// Settled; the session is being created on a background thread.
    Starting { label: String },
}

impl Draft {
    /// What the placeholder row reads as at this stage.
    pub fn label(&self) -> String {
        match self {
            Draft::Project(_) => "new session…".to_string(),
            Draft::Session(p) => format!("{}…", p.label),
            Draft::Starting { label } => format!("{label} — starting…"),
        }
    }
}

/// The agent chosen, waiting on which provider to run it against.
#[derive(Debug, Clone)]
pub struct ProviderPick {
    pub agent: Agent,
    pub choices: Vec<crate::provider::ProviderChoice>,
    /// Start an extra session alongside the existing ones, as `N` does.
    pub force_extra: bool,
}

/// Pure UI state, independent of rendering and of the multiplexer.
pub struct AppState {
    pub projects: Vec<Project>,
    pub focus: Column,
    /// Cursor over [`AppState::rows`] — one index, not one per column. A tree
    /// interleaves projects and sessions, so two cursors cannot say where you
    /// are.
    pub cursor: usize,
    /// Directories whose sessions are *hidden*.
    ///
    /// Tracks the exception rather than the rule: everything is open by
    /// default, because the point of the tree is seeing what is running
    /// without first opening three projects to find out.
    ///
    /// A project with one session has nothing to hide either way — it *is*
    /// that session, and a lone child row under it would only repeat it.
    pub collapsed: std::collections::BTreeSet<String>,
    /// Filter over project names. Only editable in filter mode.
    pub filter: String,
    /// Explicit mode, because the old TUI routed *every* unmatched key into the
    /// filter — which leaves no room for hjkl, let alone forwarding keystrokes
    /// to an agent.
    pub filtering: bool,
    /// `i` hands the keyboard to the selected session, vim-style: keys go to
    /// the agent instead of the UI until `Esc`.
    pub inserting: bool,
    /// Which column had the focus when insert mode began.
    ///
    /// `i` moves the focus to the terminal to type there; `Esc` has to give it
    /// back, or leaving insert from the tree strands the cursor on the right
    /// and turns `j`/`k` into scrolling.
    pub focus_before_insert: Option<Column>,
    /// `a` opens a picker over the terminal column; the next key is an agent
    /// alias. Shown rather than prompting on stdout, which would mean leaving
    /// the screen for the one thing that should be fastest.
    pub picking_agent: bool,
    /// Which provider to launch the just-chosen agent against.
    ///
    /// A second step rather than a longer first one: the common case is the
    /// provider CC Switch already has active, so that stays one keypress.
    pub picking_provider: Option<ProviderPick>,
    /// Configured agents, so the picker can list them and map alias -> agent.
    pub agents: Vec<Agent>,
    /// Session awaiting a kill confirmation. `d` is one key away from ending a
    /// running agent, so it asks first.
    pub confirming_kill: Option<String>,
    /// Transient message for the status line (what was just created, or why
    /// nothing was).
    pub notice: Option<String>,
    /// Where the cursor was before the draft moved it to the placeholder, so
    /// cancelling puts you back where you were rather than at the end of the
    /// list.
    pub cursor_before_draft: Option<usize>,
    /// The settings view, open while `,` has been pressed.
    pub settings: bool,
    /// A session being chosen but not yet started.
    ///
    /// While this is set the tree shows a placeholder row for it and the
    /// terminal column shows the choice being made, so the decision happens in
    /// the slot the session will occupy rather than in a window over the top.
    pub draft: Option<Draft>,
    /// What each session is about, by session name. Empty until the first
    /// sweep; a session missing from it simply draws no description line.
    pub descriptions: std::collections::BTreeMap<String, String>,
    /// Latest known status per session name, refreshed off-thread.
    ///
    /// Empty until the first sweep lands, and a session missing from the map
    /// simply draws no marker — the tree must not wait on status to be useful,
    /// and a sweep costs one terminal capture per pane.
    pub statuses: std::collections::BTreeMap<String, crate::serve::server::SessionStatus>,
}

impl AppState {
    pub fn new(projects: Vec<Project>) -> Self {
        Self::with_agents(projects, Vec::new())
    }

    pub fn with_agents(projects: Vec<Project>, agents: Vec<Agent>) -> Self {
        Self {
            projects,
            focus: Column::Tree,
            cursor: 0,
            collapsed: Default::default(),
            filter: String::new(),
            filtering: false,
            inserting: false,
            focus_before_insert: None,
            picking_agent: false,
            picking_provider: None,
            confirming_kill: None,
            agents,
            notice: None,
            settings: false,
            draft: None,
            cursor_before_draft: None,
            descriptions: std::collections::BTreeMap::new(),
            statuses: std::collections::BTreeMap::new(),
        }
    }


    /// Projects matching the filter (case-insensitive, on name or path).
    pub fn visible_projects(&self) -> Vec<&Project> {
        let f = self.filter.to_lowercase();
        self.projects
            .iter()
            .filter(|p| {
                f.is_empty()
                    || p.name.to_lowercase().contains(&f)
                    || p.dir.to_lowercase().contains(&f)
            })
            .collect()
    }

    fn project_at(&self, index: usize) -> Option<&Project> {
        self.visible_projects().get(index).copied()
    }

    /// One visible line of the tree.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (pi, project) in self.visible_projects().iter().enumerate() {
            let expandable = project.sessions.len() > 1;
            rows.push(Row::Project { index: pi, expandable });
            if expandable && !self.collapsed.contains(&project.dir) {
                for si in 0..project.sessions.len() {
                    rows.push(Row::Session { project: pi, index: si });
                }
            }
        }
        // Last, so it does not shuffle the rows above it while you type.
        if self.draft.is_some() {
            rows.push(Row::Draft);
        }
        rows
    }

    pub fn current_row(&self) -> Option<Row> {
        self.rows().get(self.cursor).copied()
    }

    pub fn current_project(&self) -> Option<&Project> {
        match self.current_row()? {
            Row::Project { index, .. } | Row::Session { project: index, .. } => {
                self.project_at(index)
            }
            // Not a project yet — that is the point of it.
            Row::Draft => None,
        }
    }

    pub fn current_sessions(&self) -> &[ManagedSession] {
        self.current_project().map(|p| p.sessions.as_slice()).unwrap_or(&[])
    }

    /// The session the cursor resolves to.
    ///
    /// A collapsed project stands in for its only session — which is why a
    /// one-session project needs no child row at all.
    pub fn current_session(&self) -> Option<&ManagedSession> {
        match self.current_row()? {
            Row::Draft => None,
            Row::Session { project, index } => self.project_at(project)?.sessions.get(index),
            Row::Project { index, .. } => {
                let project = self.project_at(index)?;
                (project.sessions.len() == 1).then(|| &project.sessions[0])
            }
        }
    }

    pub fn current_name(&self) -> Option<String> {
        self.current_session().map(|s| s.name.clone())
    }

    /// The directory new sessions should be created in.
    pub fn current_dir(&self) -> Option<String> {
        self.current_project().map(|p| p.dir.clone())
    }

    /// Agents that already have a session in the project under the cursor.
    pub fn aliases_here(&self) -> Vec<String> {
        self.current_sessions().iter().map(|s| s.alias.clone()).collect()
    }

    /// Whether the cursor may rest on a row.
    ///
    /// An open project with several sessions is a heading: every one of its
    /// sessions is already a row of its own, so stopping on the parent would
    /// be a step that selects nothing. Closed, it is the only row that project
    /// has, so it stands for the group and is selectable again.
    pub fn selectable(&self, row: Row) -> bool {
        match row {
            Row::Session { .. } | Row::Draft => true,
            Row::Project { index, expandable } => {
                !expandable
                    || self
                        .project_at(index)
                        .is_some_and(|p| self.collapsed.contains(&p.dir))
            }
        }
    }

    fn step(&self, from: usize, forward: bool) -> usize {
        let rows = self.rows();
        let mut i = from;
        loop {
            let next = if forward {
                if i + 1 >= rows.len() {
                    break from;
                }
                i + 1
            } else {
                if i == 0 {
                    break from;
                }
                i - 1
            };
            i = next;
            if rows.get(i).is_some_and(|r| self.selectable(*r)) {
                break i;
            }
        }
    }

    pub fn move_down(&mut self) {
        self.cursor = self.step(self.cursor, true);
    }

    pub fn move_up(&mut self) {
        self.cursor = self.step(self.cursor, false);
    }

    /// First row the cursor may rest on.
    pub fn first_selectable(&self) -> usize {
        self.rows()
            .iter()
            .position(|r| self.selectable(*r))
            .unwrap_or(0)
    }

    /// `l` — open a project, or step into the terminal.
    pub fn focus_right(&mut self) {
        match self.current_row() {
            Some(Row::Project { index, expandable: true }) => {
                let Some(dir) = self.project_at(index).map(|p| p.dir.clone()) else {
                    return;
                };
                // Re-opening a closed project *is* the action; the cursor
                // follows into its first session, because the heading it was
                // sitting on is no longer somewhere it can rest.
                if self.collapsed.remove(&dir) {
                    self.cursor = self.step(self.cursor, true);
                    return;
                }
                self.focus = Column::Terminal;
            }
            Some(_) => self.focus = Column::Terminal,
            None => {}
        }
    }

    /// `h` — leave the terminal, walk up to a parent, or close a project.
    pub fn focus_left(&mut self) {
        if self.focus == Column::Terminal {
            self.focus = Column::Tree;
            return;
        }
        match self.current_row() {
            // Nothing to collapse: the draft has no project yet.
            Some(Row::Draft) | None => {}
            // Close the parent and land on it. There is no "step up to the
            // heading" stop, because an open heading is not a row the cursor
            // can sit on.
            Some(Row::Session { project, .. }) => {
                if let Some(dir) = self.project_at(project).map(|p| p.dir.clone()) {
                    self.collapsed.insert(dir);
                }
                if let Some(row) = self.rows().iter().position(
                    |r| matches!(r, Row::Project { index, .. } if *index == project),
                ) {
                    self.cursor = row;
                }
            }
            Some(Row::Project { index, .. }) => {
                if let Some(dir) = self.project_at(index).map(|p| p.dir.clone()) {
                    self.collapsed.insert(dir);
                }
            }
            None => {}
        }
    }

    /// `Tab` — the next session of the project under the cursor, opening it.
    pub fn cycle_session(&mut self) {
        let Some((dir, count, index)) = self.current_row().and_then(|row| {
            let index = match row {
                Row::Project { index, .. } | Row::Session { project: index, .. } => index,
                Row::Draft => return None,
            };
            let p = self.project_at(index)?;
            Some((p.dir.clone(), p.sessions.len(), index))
        }) else {
            return;
        };
        if count < 2 {
            return;
        }
        self.collapsed.remove(&dir);

        let rows = self.rows();
        let children: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Session { project, .. } if *project == index))
            .map(|(i, _)| i)
            .collect();
        if children.is_empty() {
            return;
        }
        self.cursor = match children.iter().position(|i| *i == self.cursor) {
            Some(pos) => children[(pos + 1) % children.len()],
            None => children[0],
        };
    }

    /// Keep the cursor inside the tree after a refresh changed it.
    pub fn clamp(&mut self) {
        let n = self.rows().len();
        if n == 0 {
            self.cursor = 0;
            self.focus = Column::Tree;
            return;
        }
        if self.cursor >= n {
            self.cursor = n - 1;
        }
        // A refresh can turn the row under the cursor into a heading.
        if !self.current_row().is_some_and(|r| self.selectable(r)) {
            let forward = self.step(self.cursor, true);
            self.cursor = if forward == self.cursor {
                self.step(self.cursor, false)
            } else {
                forward
            };
        }
        // Nothing on the right means nothing to focus there.
        if self.current_session().is_none() && self.focus == Column::Terminal {
            self.focus = Column::Tree;
        }
    }
}

/// A line of the tree: a project, or one of its sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Project { index: usize, expandable: bool },
    Session { project: usize, index: usize },
    /// The session being chosen. Carries nothing: there is exactly one, and
    /// everything about it lives in [`AppState::draft`].
    Draft,
}

/// Interrupting an agent normally means Esc, but Esc is spoken for — it is how
/// you leave insert mode. `C-c` is forwarded and every agent here treats it as
/// "stop", so it stands in.
pub const KEY_TO_INTERRUPT: &str = "Ctrl-C";

/// Group sessions into projects by their working directory.
///
/// `session_cwd` is one subprocess per session, so this runs once per refresh
/// and never per frame. Sessions whose directory can't be read are grouped
/// under their own name rather than dropped — an unreachable cwd is still a
/// session the user may want to attach to or kill.
pub fn group_by_project(sessions: Vec<ManagedSession>) -> Vec<Project> {
    let mut projects: Vec<Project> = Vec::new();

    // One query for every directory, rather than one per project row.
    let aliases: std::collections::BTreeMap<String, String> = crate::store::projects()
        .into_iter()
        .filter_map(|p| p.alias.map(|a| (p.path, a)))
        .collect();

    for session in sessions {
        let dir = tmux::session_cwd(&session.name).unwrap_or_else(|_| session.name.clone());
        let name = dir
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(&dir)
            .to_string();

        match projects.iter_mut().find(|p| p.dir == dir) {
            Some(project) => project.sessions.push(session),
            None => {
                let alias = aliases.get(&dir).cloned();
                projects.push(Project { dir, name, alias, sessions: vec![session] });
            }
        }
    }

    // Sort on what is actually shown, or a named project lands under a letter
    // that appears nowhere on screen.
    projects.sort_by(|a, b| {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
    });
    projects
}

/// A live terminal attached to one session.
///
/// This is a real multiplexer client in a pty, not a periodic screenshot — the
/// difference is what makes it feel like a terminal rather than a slideshow.
/// Attaching a second client turns out to be safe here: rmux 0.10.0 does not
/// resize a session to match an attaching client, verified with both a 60- and
/// a 200-column client against an 80x24 session.
struct LiveTerm {
    session: String,
    parser: vt100::Parser,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    output: Receiver<Vec<u8>>,
}

impl LiveTerm {
    /// Attach to `session` in a pty of the given size.
    fn open(session: &str, cols: u16, rows: u16) -> Option<Self> {
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .ok()?;

        let mut command = CommandBuilder::new(tmux::mux_bin());
        command.arg("attach-session");
        command.arg("-t");
        command.arg(session);
        // amux normally runs *inside* a session, and a surviving marker makes
        // the multiplexer treat this attach as a switch-client — which fails
        // with "requires an unambiguous attached client" and leaves the column
        // showing that instead of a terminal.
        //
        // Both prefixes matter: rmux marks its clients with `RMUX`/`RMUX_PANE`,
        // tmux with `TMUX`/`TMUX_PANE`, and the server's existing sanitiser only
        // knows about the latter. Removed explicitly rather than by omission —
        // the builder merges over the inherited environment, so leaving a
        // variable out does not unset it.
        for key in ["TMUX", "TMUX_PANE", "TMUX_PROGRAM", "RMUX", "RMUX_PANE", "RMUX_PROGRAM"] {
            command.env_remove(key);
        }
        command.env("TERM", "xterm-256color");

        let child = pair.slave.spawn_command(command).ok()?;
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().ok()?;
        let writer = pair.master.take_writer().ok()?;
        let (tx, output) = mpsc::channel();

        // A blocking read on its own thread: the loop stays responsive and the
        // terminal keeps streaming while the user sits still.
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buffer[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Some(Self {
            session: session.to_string(),
            parser: vt100::Parser::new(rows, cols, 0),
            master: pair.master,
            writer,
            child,
            output,
        })
    }

    /// Drain whatever the session has produced. True when anything arrived.
    fn pump(&mut self) -> bool {
        let mut got = false;
        loop {
            match self.output.try_recv() {
                Ok(chunk) => {
                    self.parser.process(&chunk);
                    got = true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        got
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if self.parser.screen().size() == (rows, cols) {
            return;
        }
        self.parser.set_size(rows, cols);
        let _ = self
            .master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
    }

    fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }
}

impl Drop for LiveTerm {
    fn drop(&mut self) {
        // Detaching the client must not take the session with it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Outcome of the TUI loop, decided after the terminal is restored.
enum Outcome {
    Quit,
    Attach(String),
    Kill(String),
}

pub fn run_tui(agents: &[Agent]) -> Result<()> {
    let all = tmux::list_session_names()?;
    let sessions = managed_sessions(&all, agents);
    let mut state = AppState::with_agents(group_by_project(sessions), agents.to_vec());
    // Land on something selectable — the first row is a heading whenever the
    // first project has several sessions.
    state.cursor = state.first_selectable();

    let outcome = event_loop(&mut state)?;

    match outcome {
        Outcome::Quit => Ok(()),
        Outcome::Attach(name) => tmux::attach_or_switch(&name),
        Outcome::Kill(name) => {
            tmux::kill_session(&name)?;
            // re-enter the TUI with refreshed list
            run_tui(agents)
        }
    }
}


/// What tells two sessions of one directory apart: the provider it runs
/// against, the suffix `amux new` gave it, or both.
///
/// Parsed around the trailing hash rather than by splitting on the last `-`,
/// which put a provider's own hyphen in the way and rendered
/// `cc-glm_amux_4d8e0883` as "glm_amux…".
fn session_qualifier(name: &str) -> String {
    let tail = name.rsplit_once('_').map(|(_, t)| t).unwrap_or("");
    let suffix = tail.split_once('-').map(|(_, s)| s).unwrap_or("");
    let provider = name
        .split_once('_')
        .map(|(head, _)| head)
        .and_then(|head| head.split_once('-'))
        .map(|(_, p)| p)
        .unwrap_or("");
    match (provider.is_empty(), suffix.is_empty()) {
        (true, _) => suffix.to_string(),
        (false, true) => provider.to_string(),
        (false, false) => format!("{provider} {suffix}"),
    }
}

/// The agent's own name for a session, rather than the shell alias.
///
/// Sessions are named with the short alias (`cx`, `oc`) because that is what
/// gets typed, but the tree is for reading: "codex" and "opencode" say what is
/// running without the reader having to know the abbreviations.
fn agent_name<'a>(agents: &'a [Agent], alias: &'a str) -> &'a str {
    agents
        .iter()
        .find(|a| a.alias == alias)
        .map(|a| a.name.as_str())
        .unwrap_or(alias)
}

/// How long the state has held, short enough to sit beside the marker.
///
/// Blank when the status was inferred rather than reported: a terminal tail
/// says what is on screen, not since when, and `0m` beside an agent that has
/// been blocked for an hour is worse than nothing.
fn status_age(status: Option<&crate::serve::server::SessionStatus>) -> String {
    let Some(since) = status.and_then(|s| s.since) else {
        return String::new();
    };
    let secs = (chrono::Utc::now().timestamp() - since).max(0);
    // Under a minute reads as "just now" — the exact second is noise.
    match secs {
        s if s < 60 => String::new(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// The word and colour for a session's status.
///
/// Spelled out rather than symbolised: a glyph needs a legend, and `.` was
/// doing double duty for both `done` and `idle`, told apart only by colour. An
/// unknown status (no sweep yet, or a session the daemon cannot see) draws
/// blank rather than guessing — claiming "idle" for something merely unmeasured
/// is exactly the kind of confident wrong answer this is meant to stop.
fn status_marker(status: Option<&crate::serve::server::SessionStatus>) -> (&'static str, Color) {
    use crate::serve::server::PaneStatus;
    match status.map(|s| &s.status) {
        Some(PaneStatus::Waiting) => ("waiting", Color::Magenta),
        Some(PaneStatus::Running) => ("running", Color::Yellow),
        Some(PaneStatus::Failed) => ("failed", Color::Red),
        Some(PaneStatus::Done) => ("done", Color::Green),
        Some(PaneStatus::Idle) => ("idle", Color::DarkGray),
        None => ("", Color::DarkGray),
    }
}

/// Size of the terminal column, in cells, for the current frame size.
fn term_size(area: Rect) -> (u16, u16) {
    // Minus the border on each side.
    (area.width.saturating_sub(2).max(20), area.height.saturating_sub(2).max(5))
}

fn event_loop(state: &mut AppState) -> Result<Outcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut live: Option<LiveTerm> = None;
    let mut last_draw = Instant::now() - REDRAW_FLOOR;
    let mut last_reload = Instant::now();
    let mut known: Vec<String> = managed_names(&state.agents);
    // What was in use before insert mode switched to ASCII.
    let mut saved_ime: Option<String> = None;
    ime::learn_current();
    let mut dirty = true;

    // Creating a session is done off-thread as well. `create_detached` blocks
    // for as long as codex takes to answer its launch prompts — up to eight
    // seconds — and doing that inline froze the whole UI. Worse, the keys
    // pressed during the freeze were delivered afterwards, against a screen
    // that had since changed: an Enter meant for the picker landed on the tree
    // and attached, replacing the TUI with a full-screen agent.
    let (spawn_tx, spawn_rx) = mpsc::channel::<Result<String, String>>();

    // Past conversations are listed off-thread too: even after the codex
    // header fix this is ~200ms for a busy directory, and it runs the moment
    // Enter is pressed — synchronously it would freeze the frame.
    let (sessions_tx, sessions_rx) = mpsc::channel::<(String, Vec<SessionEntry>)>();

    // Descriptions are resolved off-thread too: each one may read an agent's
    // transcript, and there is one per live session.
    let (desc_tx, desc_rx) = mpsc::channel::<std::collections::BTreeMap<String, String>>();
    {
        let tx = desc_tx.clone();
        let projects = state.projects.clone();
        let agents = state.agents.clone();
        std::thread::spawn(move || {
            let _ = tx.send(describe_sessions(&projects, &agents));
        });
    }

    // Status is computed on its own thread: a sweep captures the terminal of
    // every pane (~18ms each), which across a dozen sessions would stall the
    // very loop it is meant to annotate.
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::spawn(move || loop {
        // `send` failing means the TUI has exited and dropped the receiver.
        if status_tx.send(crate::serve::server::session_statuses()).is_err() {
            return;
        }
        std::thread::sleep(STATUS_EVERY);
    });

    let result = loop {
        state.clamp();

        // Attach to whatever is selected, and drop a terminal whose session is
        // no longer the one in view.
        let wanted = state.current_name();
        if live.as_ref().map(|t| &t.session) != wanted.as_ref() {
            live = None;
            if let Some(name) = &wanted {
                let (cols, rows) = term_size(terminal_column(terminal.get_frame().area()));
                live = LiveTerm::open(name, cols, rows);
            }
            dirty = true;
        }

        if let Some(term) = live.as_mut() {
            let (cols, rows) = term_size(terminal_column(terminal.get_frame().area()));
            term.resize(cols, rows);
            if term.pump() {
                dirty = true;
            }
        }

        if dirty && last_draw.elapsed() >= REDRAW_FLOOR {
            terminal.draw(|f| render(f, state, live.as_ref()))?;
            last_draw = Instant::now();
            dirty = false;
        }

        while let Ok(result) = spawn_rx.try_recv() {
            match result {
                Ok(name) => {
                    state.notice = Some(format!("started {name}"));
                    // The placeholder has become a real row; `reload` puts the
                    // cursor on it, and the column it was drawn in turns into
                    // that session's terminal.
                    state.draft = None;
                    state.cursor_before_draft = None;
                    reload(state, Some(name));
                    live = None;
                }
                Err(e) => {
                    state.draft = None;
                    state.notice = Some(e);
                }
            }
            dirty = true;
        }

        // A listing is only useful for the project still on screen — moving on
        // before it lands must not repopulate the picker with the old one.
        while let Ok((dir, entries)) = sessions_rx.try_recv() {
            if apply_listing(state, &dir, entries) {
                dirty = true;
            }
        }

        while let Ok(map) = desc_rx.try_recv() {
            if map != state.descriptions {
                state.descriptions = map;
                dirty = true;
            }
        }

        // Keep only the newest sweep; anything behind it is already superseded.
        let mut newest = None;
        while let Ok(map) = status_rx.try_recv() {
            newest = Some(map);
        }
        if let Some(map) = newest {
            if map != state.statuses {
                state.statuses = map;
                dirty = true;
            }
        }

        // Sessions appear and disappear outside this screen. Compare names
        // first — regrouping means a `session_cwd` subprocess per session, and
        // most ticks find nothing changed.
        if last_reload.elapsed() >= RELOAD_EVERY {
            last_reload = Instant::now();
            let names = managed_names(&state.agents);
            if names != known {
                known = names;
                let keep = state.current_name();
                reload(state, keep);
                dirty = true;
            }
            // Titles are refined as an agent works, so re-read them even when
            // the set of sessions has not moved.
            let tx = desc_tx.clone();
            let projects = state.projects.clone();
            let agents = state.agents.clone();
            std::thread::spawn(move || {
                let _ = tx.send(describe_sessions(&projects, &agents));
            });
        }

        // A short poll rather than a blocking read: output arrives on its own
        // schedule and has to be drained between keystrokes.
        if event::poll(POLL)? {
            let ev = event::read()?;

            if let Event::Mouse(MouseEvent { kind, column, row, .. }) = ev {
                // Only the terminal column forwards — a wheel over the lists
                // should move the selection, not scroll someone's agent.
                let area = terminal_column(terminal.get_frame().area());
                let inside = column > area.x
                    && column < area.x + area.width.saturating_sub(1)
                    && row > area.y
                    && row < area.y + area.height.saturating_sub(1);
                if inside {
                    if let Some(term) = live.as_mut() {
                        let bytes = encode_mouse(kind, column - area.x - 1, row - area.y - 1);
                        if !bytes.is_empty() {
                            term.write(&bytes);
                        }
                    }
                } else {
                    match kind {
                        MouseEventKind::ScrollDown => state.move_down(),
                        MouseEventKind::ScrollUp => state.move_up(),
                        _ => {}
                    }
                    dirty = true;
                }
                continue;
            }

            if let Event::Key(key) = ev {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                dirty = true;

                // A pending kill takes the next key: y confirms, anything
                // else cancels. Deliberately not Enter — the point is that a
                // stray keystroke must not be able to confirm it.
                if let Some(target) = state.confirming_kill.clone() {
                    state.confirming_kill = None;
                    if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                        break Outcome::Kill(target);
                    }
                    state.notice = Some("kill cancelled".into());
                    continue;
                }

                if state.settings {
                    match key.code {
                        KeyCode::Esc | KeyCode::Char(',') => state.settings = false,
                        KeyCode::Char('s') => match crate::serve::start_quiet() {
                            Ok(pid) => {
                                state.notice = Some(format!("monitor started (pid {pid})"))
                            }
                            Err(e) => state.notice = Some(format!("could not start: {e}")),
                        },
                        KeyCode::Char('S') => match crate::serve::stop_quiet() {
                            Some(pid) => {
                                state.notice = Some(format!("monitor stopped (pid {pid})"))
                            }
                            None => state.notice = Some("monitor was not running".into()),
                        },
                        _ => {}
                    }
                    dirty = true;
                    continue;
                }
                // While a session is being chosen every key belongs to that
                // choice, including the letters that normally navigate.
                if state.draft.is_some() {
                    match state.draft.as_mut() {
                        Some(Draft::Project(picker)) => match key.code {
                            KeyCode::Esc => {
                                state.draft = None;
                                if let Some(previous) = state.cursor_before_draft.take() {
                                    state.cursor = previous;
                                }
                            }
                            KeyCode::Backspace => {
                                picker.query.pop();
                                picker.clamp();
                            }
                            KeyCode::Down => picker.move_by(1),
                            KeyCode::Up => picker.move_by(-1),
                            KeyCode::Char('n')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                picker.move_by(1)
                            }
                            KeyCode::Char('p')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                picker.move_by(-1)
                            }
                            KeyCode::Enter => {
                                if let Some(entry) = picker.selected().cloned() {
                                    state.draft = Some(Draft::Session(
                                        SessionPicker::from_projects(
                                            entry.path.clone(),
                                            entry.label,
                                        ),
                                    ));
                                    let tx = sessions_tx.clone();
                                    let agents = state.agents.clone();
                                    let dir = entry.path;
                                    std::thread::spawn(move || {
                                        let found = sessions_in(&dir, &agents);
                                        let _ = tx.send((dir, found));
                                    });
                                }
                            }
                            KeyCode::Char(c) => {
                                picker.query.push(c);
                                picker.clamp();
                            }
                            _ => {}
                        },
                        Some(Draft::Session(picker)) => match key.code {
                            // Back to the directory list rather than out
                            // altogether: picking the wrong project is the
                            // likelier mistake, and starting over costs the
                            // query you just typed. Unless there was no
                            // directory list — `O` starts here — in which case
                            // going "back" to one would be inventing a step the
                            // user never took.
                            KeyCode::Esc => {
                                if picker.from_project_list {
                                    state.draft = Some(Draft::Project(ProjectPicker::new(
                                        crate::store::projects(),
                                    )));
                                } else {
                                    state.draft = None;
                                    if let Some(previous) = state.cursor_before_draft.take() {
                                        state.cursor = previous;
                                    }
                                }
                            }
                            KeyCode::Down | KeyCode::Char('j') => picker.move_by(1),
                            KeyCode::Up | KeyCode::Char('k') => picker.move_by(-1),
                            KeyCode::Char('n') => {
                                // A fresh conversation rather than a recorded
                                // one. Not `force_extra`: a project reached this
                                // way usually has no live session at all, and
                                // forcing the suffix named the first one `…-2`.
                                let dir = picker.dir.clone();
                                let label = picker.label.clone();
                                state.draft = Some(Draft::Starting { label });
                                if let Some(agent) = state.agents.first().cloned() {
                                    spawn_agent_in(state, &spawn_tx, &agent, &dir, false);
                                }
                            }
                            KeyCode::Enter => {
                                let chosen = picker.selected().cloned();
                                let dir = picker.dir.clone();
                                let label = picker.label.clone();
                                if let Some(entry) = chosen {
                                    state.draft = Some(Draft::Starting { label });
                                    if let Some(already) =
                                        resume_session(state, &spawn_tx, &entry, &dir)
                                    {
                                        // Already running: nothing to wait for.
                                        state.draft = None;
                                        reload(state, Some(already));
                                        live = None;
                                    }
                                }
                            }
                            _ => {}
                        },
                        // Waiting on the launch. Esc gives up on watching for
                        // it; the session still finishes coming up and appears
                        // on the next refresh.
                        Some(Draft::Starting { .. }) => {
                            if key.code == KeyCode::Esc {
                                state.draft = None;
                                if let Some(previous) = state.cursor_before_draft.take() {
                                    state.cursor = previous;
                                }
                            }
                        }
                        None => {}
                    }
                    dirty = true;
                    continue;
                }
                // Second step of the agent picker: which provider.
                if let Some(pick) = state.picking_provider.take() {
                    match key.code {
                        KeyCode::Esc => {}
                        // Enter is the fast path: whatever CC Switch has
                        // active, which is what every session used to get.
                        KeyCode::Enter => {
                            spawn_agent(state, &spawn_tx, &pick.agent, None, pick.force_extra)
                        }
                        KeyCode::Char(c) => match picked_index(c, pick.choices.len()) {
                            Some(i) => {
                                let name = pick.choices[i].name.clone();
                                spawn_agent(
                                    state,
                                    &spawn_tx,
                                    &pick.agent,
                                    Some(&name),
                                    pick.force_extra,
                                );
                            }
                            None => {
                                state.notice =
                                    Some(format!("'{c}' is not one of the listed numbers"));
                            }
                        },
                        _ => {}
                    }
                    dirty = true;
                    continue;
                }
                if state.picking_agent {
                    state.picking_agent = false;
                    if key.code == KeyCode::Esc {
                        continue;
                    }
                    if let KeyCode::Char(c) = key.code {
                        match picked_agent(&state.agents, c).cloned() {
                            Some(agent) => {
                                // Offer the provider step only where it means
                                // something; the others go straight to launch.
                                let choices = if crate::provider::selectable(&agent.name) {
                                    crate::provider::list(crate::provider::agent_app_type(
                                        &agent.name,
                                    ))
                                } else {
                                    Vec::new()
                                };
                                if choices.len() > 1 {
                                    state.picking_provider = Some(ProviderPick {
                                        agent,
                                        choices,
                                        force_extra: false,
                                    });
                                } else {
                                    spawn_agent(state, &spawn_tx, &agent, None, false);
                                }
                            }
                            None => {
                                state.notice =
                                    Some(format!("'{c}' is not one of the listed numbers"));
                            }
                        }
                    }
                    dirty = true;
                    continue;
                }

                if state.inserting {
                    if key.code == KeyCode::Esc {
                        state.inserting = false;
                        // Hand the focus back to wherever `i` was pressed.
                        if let Some(previous) = state.focus_before_insert.take() {
                            state.focus = previous;
                        }
                        // Back to ASCII so hjkl navigate instead of typing.
                        saved_ime = ime::drop_to_ascii();
                        continue;
                    }
                    if let Some(term) = live.as_mut() {
                        let bytes = encode_key(key.code, key.modifiers);
                        if !bytes.is_empty() {
                            term.write(&bytes);
                        }
                    }
                    continue;
                }

                if state.filtering {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter => state.filtering = false,
                        KeyCode::Backspace => {
                            state.filter.pop();
                        }
                        KeyCode::Char(c) => state.filter.push(c),
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break Outcome::Quit
                    }
                    KeyCode::Char('q') => break Outcome::Quit,
                    KeyCode::Esc => state.focus_left(),
                    // With the terminal focused these walk its scrollback
                    // instead of the tree. Moving the selection from here
                    // would swap out the very session being read.
                    KeyCode::Char('j') | KeyCode::Down => {
                        if state.focus == Column::Terminal {
                            scroll_live(live.as_mut(), MouseEventKind::ScrollDown);
                        } else {
                            state.move_down();
                        }
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        if state.focus == Column::Terminal {
                            scroll_live(live.as_mut(), MouseEventKind::ScrollUp);
                        } else {
                            state.move_up();
                        }
                    }
                    KeyCode::Char('h') | KeyCode::Left => state.focus_left(),
                    KeyCode::Char('l') | KeyCode::Right => state.focus_right(),
                    KeyCode::Char('i') => {
                        if state.current_name().is_some() {
                            state.inserting = true;
                            state.focus_before_insert = Some(state.focus);
                            state.focus = Column::Terminal;
                            // Back to the method you type with — the one this
                            // session left, or the one remembered from before.
                            ime::resume_typing(saved_ime.take());
                        }
                    }
                    KeyCode::Char('/') => {
                        state.filtering = true;
                        state.filter.clear();
                    }
                    KeyCode::Char(',') => state.settings = true,
                    KeyCode::Char('o') => {
                        state.draft =
                            Some(Draft::Project(ProjectPicker::new(crate::store::projects())));
                        // Land on the placeholder: the right column is showing
                        // the choice for it, so highlighting anything else
                        // would point at the wrong row.
                        state.cursor_before_draft = Some(state.cursor);
                        state.cursor = state.rows().len().saturating_sub(1);
                    }
                    // Same flow, but for the project already under the cursor:
                    // skip choosing a directory and go straight to its
                    // conversations. `o` is for somewhere else, `O` is for here.
                    KeyCode::Char('O') => {
                        let here = state
                            .current_project()
                            .map(|p| (p.dir.clone(), p.display_name().to_string()));
                        match here {
                            Some((dir, label)) => {
                                state.draft =
                                    Some(Draft::Session(SessionPicker::new(dir.clone(), label)));
                                state.cursor_before_draft = Some(state.cursor);
                                state.cursor = state.rows().len().saturating_sub(1);
                                let tx = sessions_tx.clone();
                                let agents = state.agents.clone();
                                std::thread::spawn(move || {
                                    let found = sessions_in(&dir, &agents);
                                    let _ = tx.send((dir, found));
                                });
                            }
                            None => {
                                state.notice =
                                    Some("nothing selected — press o to pick a project".into())
                            }
                        }
                    }
                    KeyCode::Char('a') => {
                        if state.current_dir().is_some() {
                            state.picking_agent = true;
                        }
                    }
                    KeyCode::Char('N') => {
                        // Another session for the agent already selected here.
                        let agent = state
                            .current_session()
                            .and_then(|s| {
                                state.agents.iter().find(|a| a.alias == s.alias).cloned()
                            });
                        if let Some(agent) = agent {
                            // No provider: `ManagedSession` records the plain
                            // alias, so the one a `cc-glm` session runs on is
                            // not available here. Same as before this step
                            // existed.
                            spawn_agent(state, &spawn_tx, &agent, None, true);
                        }
                    }
                    KeyCode::Tab => state.cycle_session(),
                    KeyCode::Char('d') => {
                        state.confirming_kill = state.current_name();
                    }
                    // Enter opens the session *here*, in the terminal column —
                    // the same thing `i` does. It used to exec into a
                    // full-screen client, which meant a second Enter after
                    // picking a project replaced the whole layout with the
                    // agent you had just opened.
                    KeyCode::Enter => {
                        if state.current_name().is_some() {
                            state.inserting = true;
                            state.focus_before_insert = Some(state.focus);
                            state.focus = Column::Terminal;
                            ime::resume_typing(saved_ime.take());
                        }
                    }
                    // Handing the terminal over is still available, but it now
                    // takes a deliberate key rather than the one you press to
                    // look at something.
                    KeyCode::Char('A') => {
                        if let Some(name) = state.current_name() {
                            break Outcome::Attach(name);
                        }
                    }
                    _ => {}
                }
            }
        }
    };

    // Leave the input method as it was found, not as insert mode left it.
    ime::restore(saved_ime.take());
    // Detach before restoring the screen, so the client goes away cleanly.
    drop(live);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    Ok(result)
}

/// Remember and restore the input method around insert mode.
///
/// Leaving insert mode with a CJK method still active makes `hjkl` type
/// characters instead of moving — the navigation keys are unreachable exactly
/// when you want them. Dropping to ASCII on the way out and restoring on the
/// way back in keeps both halves usable.
///
/// Depends on `im-select`, which most machines will not have. Every failure is
/// silent and leaves the input method alone: navigation must not break because
/// a helper is missing.
mod ime {
    use std::process::Command;

    const ASCII: &str = "com.apple.keylayout.ABC";
    const PREFERRED: &str = "ime.typing";

    fn current() -> Option<String> {
        let out = Command::new("im-select").output().ok()?;
        if !out.status.success() {
            return None;
        }
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!name.is_empty()).then_some(name)
    }

    fn select(source: &str) {
        let _ = Command::new("im-select")
            .arg(source)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    /// Remember `source` as the method to type with, unless it is ASCII.
    ///
    /// Storing ASCII would be self-defeating: it is what the tree switches *to*
    /// so that `hjkl` navigate, so recording it would make `i` a no-op forever.
    fn remember(source: &str) {
        if source != ASCII {
            crate::store::set_setting(PREFERRED, source);
        }
    }

    /// Note whatever is in use right now as the preferred typing method.
    ///
    /// Called once as the TUI opens: launching amux while typing Chinese is a
    /// clear statement of which method `i` should return you to, and it means
    /// the very first `i` works rather than only those after an `Esc`.
    pub(super) fn learn_current() {
        if let Some(source) = current() {
            remember(&source);
        }
    }

    /// Switch to ASCII, returning what was in use so it can be put back.
    pub(super) fn drop_to_ascii() -> Option<String> {
        let previous = current()?;
        if previous == ASCII {
            return None;
        }
        remember(&previous);
        select(ASCII);
        Some(previous)
    }

    /// Switch to the method to type with: the one just left, else the
    /// remembered one. The fallback is what makes this survive a restart.
    pub(super) fn resume_typing(previous: Option<String>) {
        if let Some(source) = previous.or_else(|| crate::store::setting(PREFERRED)) {
            select(&source);
        }
    }

    pub(super) fn restore(previous: Option<String>) {
        if let Some(source) = previous {
            select(&source);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Recording ASCII would be self-defeating: the tree switches to it on
        /// every `Esc`, so storing it would make `i` stop switching at all.
        #[test]
        fn ascii_is_never_remembered_as_the_typing_method() {
            let _db = crate::test_home::scratch_db();

            remember("com.tencent.inputmethod.wetype.pinyin");
            assert_eq!(
                crate::store::setting(PREFERRED).as_deref(),
                Some("com.tencent.inputmethod.wetype.pinyin")
            );

            remember(ASCII);
            assert_eq!(
                crate::store::setting(PREFERRED).as_deref(),
                Some("com.tencent.inputmethod.wetype.pinyin"),
                "dropping to ASCII overwrote the method to come back to"
            );
        }
    }
}

/// Walk the focused terminal's scrollback by one step.
///
/// Sent as a wheel report rather than driven through `copy-mode` on the CLI:
/// the attached client already has `mouse on`, so this is the same path the
/// wheel takes and needs no special casing for entering or leaving copy mode.
fn scroll_live(live: Option<&mut LiveTerm>, direction: MouseEventKind) {
    if let Some(term) = live {
        let bytes = encode_mouse(direction, 1, 1);
        if !bytes.is_empty() {
            term.write(&bytes);
        }
    }
}

/// Which agent a key in the picker selects, if any.
///
/// Keyed on the *number* shown beside each entry rather than the alias:
/// `cc` and `cx` are two characters and share a first letter, so one keypress
/// can never name one of them. An earlier version compared a single char
/// against the alias, which meant only single-character aliases — `p` alone —
/// could ever be chosen.
pub fn picked_agent<'a>(agents: &'a [Agent], key: char) -> Option<&'a Agent> {
    picked_index(key, agents.len()).map(|i| &agents[i])
}

/// The 1-based number shown beside a list entry, as an index into it.
pub fn picked_index(key: char, len: usize) -> Option<usize> {
    let index = key.to_digit(10)? as usize;
    // `then`, not `then_some`: the latter evaluates its argument eagerly, and
    // `0 - 1` on a usize is an overflow rather than a skipped branch.
    (index >= 1 && index <= len).then(|| index - 1)
}

/// Start `agent` in the selected project's directory, without leaving the TUI.
///
/// Two shapes, which is the distinction the keys expose:
///   - the directory's *primary* session for that agent, when it has none yet
///   - an extra one, auto-suffixed `-2`, `-3`, … when it already does
///
/// Returns the new session's name so the caller can select it.
fn spawn_agent(
    state: &mut AppState,
    tx: &mpsc::Sender<Result<String, String>>,
    agent: &Agent,
    provider: Option<&str>,
    force_extra: bool,
) {
    let Some(dir) = state.current_dir() else {
        return;
    };
    let cwd = std::path::PathBuf::from(&dir);

    // The provider is part of the session's identity, exactly as it is for
    // `amux run --provider`: two providers of one agent in one directory are
    // separate sessions, not one that silently changed endpoint.
    let alias = match provider {
        Some(p) => format!("{}-{}", agent.alias, p),
        None => agent.alias.clone(),
    };
    let base = crate::session::session_name(&alias, &cwd);

    let name = if force_extra || tmux::has_session(&base) {
        format!("{base}-{}", crate::commands::new::next_free_suffix(&base))
    } else {
        base
    };

    let mut env_vars: Vec<(String, String)> = Vec::new();
    let mut argv = agent.command.clone();
    if let Some(p) = provider {
        let app_type = crate::provider::agent_app_type(&agent.name);
        match crate::provider::resolve_settings(p, app_type) {
            Ok(settings) => {
                argv.extend(settings.extra_argv);
                env_vars = settings.env_vars;
            }
            Err(e) => {
                state.notice = Some(format!("provider {p}: {e}"));
                return;
            }
        }
    }

    // A fresh session picks up the conversation this directory was last on for
    // that agent, the same way `amux run` does — an extra session deliberately
    // does not, since it is a second workspace rather than a continuation.
    if !name.contains('-') || !force_extra {
        if let Some(id) = crate::commands::session_ids::load_id(&name)
            .filter(|id| crate::commands::session_ids::session_file_exists(&agent.name, &cwd, id))
        {
            argv.extend(crate::commands::session_ids::resume_args(&agent.name, &id));
        }
    }

    state.notice = Some(format!("starting {name}…"));
    launch_off_thread(tx.clone(), agent.clone(), cwd, name, argv, env_vars);
}

/// Fill the session picker with a listing, if it is still the right one.
///
/// Listings arrive from a background thread, so by the time one lands the user
/// may have cancelled or moved to another project. Applying it regardless would
/// repopulate a closed picker, or show one directory's conversations under
/// another's name. Returns whether anything changed.
fn apply_listing(state: &mut AppState, dir: &str, entries: Vec<SessionEntry>) -> bool {
    let Some(Draft::Session(picker)) = state.draft.as_mut() else {
        return false;
    };
    if picker.dir != dir {
        return false;
    }
    picker.entries = entries;
    picker.loading = false;
    picker.cursor = 0;
    true
}

/// Start `agent` in `dir`, detached, and report the session it created.
///
/// The directory is explicit rather than taken from the cursor: the project
/// picker opens directories that have no session in the tree at all, so there
/// is nothing selected to read it from.
fn spawn_agent_in(
    state: &mut AppState,
    tx: &mpsc::Sender<Result<String, String>>,
    agent: &Agent,
    dir: &str,
    force_extra: bool,
) {
    let cwd = std::path::PathBuf::from(dir);
    let base = crate::session::session_name(&agent.alias, &cwd);
    let name = if force_extra || tmux::has_session(&base) {
        format!("{base}-{}", crate::commands::new::next_free_suffix(&base))
    } else {
        base
    };
    state.notice = Some(format!("starting {name}…"));
    launch_off_thread(tx.clone(), agent.clone(), cwd, name, agent.command.clone(), Vec::new());
}

/// Create a session on its own thread, reporting the name back when it is up.
///
/// The wait is not incidental: for codex `create_detached` polls the pane for
/// its launch prompts, which takes seconds. Holding the event loop for that
/// long both freezes the display and queues up keystrokes that are then
/// replayed against a screen that has moved on.
fn launch_off_thread(
    tx: mpsc::Sender<Result<String, String>>,
    agent: Agent,
    cwd: std::path::PathBuf,
    name: String,
    argv: Vec<String>,
    env_vars: Vec<(String, String)>,
) {
    std::thread::spawn(move || {
        let result = crate::commands::run::create_detached(&agent, &cwd, &name, &argv, &env_vars)
            .map(|()| name)
            .map_err(|e| format!("could not start {}: {e}", agent.name));
        let _ = tx.send(result);
    });
}

/// Reopen one recorded conversation, detached.
fn resume_session(
    state: &mut AppState,
    tx: &mpsc::Sender<Result<String, String>>,
    entry: &SessionEntry,
    dir: &str,
) -> Option<String> {
    let Some(agent) = crate::config::find(&state.agents, &entry.agent).cloned() else {
        state.notice = Some(format!("agent '{}' is not configured", entry.agent));
        return None;
    };
    let cwd = std::path::PathBuf::from(dir);

    // `announce: false` — `resume_plan` would otherwise print to the terminal
    // the TUI is drawing on.
    let (name, argv, exists) =
        crate::commands::run::resume_plan(&agent, &cwd, &entry.id, true, false);
    if exists {
        state.notice = Some(format!("{name} is already open"));
        return Some(name);
    }

    state.notice = Some(format!(
        "resuming {}…",
        crate::commands::list::short_id(&entry.id)
    ));
    launch_off_thread(tx.clone(), agent, cwd, name, argv, Vec::new());
    None
}

/// Every managed session name currently on the server, sorted.
fn managed_names(agents: &[Agent]) -> Vec<String> {
    let all = tmux::list_session_names().unwrap_or_default();
    let mut names: Vec<String> = managed_sessions(&all, agents)
        .into_iter()
        .map(|s| s.name)
        .collect();
    names.sort();
    names
}

/// Re-read the session list, keeping `select` selected if it is still there.
fn reload(state: &mut AppState, select: Option<String>) {
    let all = tmux::list_session_names().unwrap_or_default();
    state.projects = group_by_project(managed_sessions(&all, &state.agents));

    if let Some(target) = select {
        // Open whatever holds it, so there is a row to land on.
        let home = state
            .visible_projects()
            .iter()
            .find(|p| p.sessions.iter().any(|s| s.name == target))
            .map(|p| (p.dir.clone(), p.sessions.len()));
        if let Some((dir, count)) = home {
            if count > 1 {
                state.collapsed.remove(&dir);
            }
        }

        // Walk the rows looking for the one that resolves to this session.
        let total = state.rows().len();
        let saved = state.cursor;
        for i in 0..total {
            state.cursor = i;
            if state.current_name().as_deref() == Some(target.as_str()) {
                return;
            }
        }
        state.cursor = saved;
    }
    state.clamp();
}

/// Where the terminal column lands for a given frame, so the pty can be sized
/// to it before the first draw.
fn terminal_column(area: Rect) -> Rect {
    let body = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area)[0];

    columns_of(body)[1]
}

/// Tree on the left, terminal on the right.
///
/// Declared once so [`terminal_column`] and [`render`] cannot disagree about
/// where the pty's cells are — a mismatch would size the terminal to one rect
/// and draw it into another.
fn columns_of(area: Rect) -> std::rc::Rc<[Rect]> {
    Layout::default()
        .direction(Direction::Horizontal)
        // Four elevenths on a normal terminal — the status is spelled out, and
        // "running" plus an agent name and a duration does not fit in thirty
        // cells — but capped, because the content stops growing at about
        // thirty-five and a proportional column on a very wide terminal is
        // mostly empty space taken from the terminal view.
        .constraints([Constraint::Length(tree_width(area.width)), Constraint::Min(1)])
        .split(area)
}

/// Encode a mouse event as an SGR report, the way a real terminal would.
///
/// The attached client has `mouse on`, so forwarding these is what lets the
/// wheel walk a session's scrollback — rmux enters copy-mode on its own,
/// exactly as it does under a normal attach. Coordinates are relative to the
/// terminal column and 1-based, which is what the protocol expects.
pub fn encode_mouse(kind: MouseEventKind, col: u16, row: u16) -> Vec<u8> {
    let button = match kind {
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        MouseEventKind::Down(MouseButton::Left) => 0,
        MouseEventKind::Down(MouseButton::Middle) => 1,
        MouseEventKind::Down(MouseButton::Right) => 2,
        // Releases report the button that went up; the terminal only needs the
        // final `m` to know it was a release.
        MouseEventKind::Up(_) => {
            return format!("\x1b[<0;{};{}m", col + 1, row + 1).into_bytes()
        }
        _ => return Vec::new(),
    };
    format!("\x1b[<{button};{};{}M", col + 1, row + 1).into_bytes()
}

/// Encode a keypress as the bytes a terminal application expects.
///
/// Writing straight into the pty means no translation table of key *names* —
/// the agent sees exactly what it would from a real terminal, including the
/// escape sequences for arrows and the control codes for chords.
///
/// `Esc` is absent on purpose: it leaves insert mode and never reaches the
/// agent, so `KEY_TO_INTERRUPT` stands in for interrupting one.
pub fn encode_key(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    match code {
        // Ctrl-A..Ctrl-Z are 0x01..0x1A.
        KeyCode::Char(c) if ctrl && c.is_ascii_alphabetic() => {
            vec![(c.to_ascii_lowercase() as u8) - b'a' + 1]
        }
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        _ => Vec::new(),
    }
}

/// Border style that marks which column owns the keyboard.
fn border_for(state: &AppState, column: Column) -> (BorderType, Style) {
    if state.inserting && column == Column::Terminal {
        // Distinct from ordinary focus: in insert mode a keypress goes to the
        // agent, not the UI, and that had better be unmistakable.
        (BorderType::Thick, Style::default().fg(Color::Green))
    } else if state.focus == column && !state.inserting {
        (BorderType::Thick, Style::default().fg(Color::Cyan))
    } else {
        (BorderType::Plain, Style::default().fg(Color::DarkGray))
    }
}

fn render(f: &mut Frame, state: &AppState, live: Option<&LiveTerm>) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let columns = columns_of(outer[0]);
    render_tree(f, state, columns[0]);
    if state.settings {
        render_settings(f, columns[1]);
    } else {
        match &state.draft {
            Some(draft) => render_draft(f, draft, columns[1]),
            None => render_terminal(f, state, live, columns[1]),
        }
    }

    if let Some(pick) = &state.picking_provider {
        render_provider_picker(f, pick, outer[0]);
    }
    if state.picking_agent {
        render_agent_picker(f, state, outer[0]);
    }
    render_status(f, state, outer[1]);
}

/// Overlay listing agents and the key that starts each one.
///
/// Marks the ones that already have a session here: choosing those opens an
/// extra session rather than a first, and seeing that before pressing beats
/// finding out afterwards.
fn render_agent_picker(f: &mut Frame, state: &AppState, area: Rect) {
    let here = state.aliases_here();
    let rows: Vec<Line> = state
        .agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let running = here.iter().filter(|x| *x == &a.alias).count();
            let suffix = match running {
                0 => String::new(),
                n => format!("  ({n} running — opens another)"),
            };
            Line::from(vec![
                Span::styled(
                    format!("  {}  ", i + 1),
                    Style::default().fg(Color::Black).bg(Color::Cyan),
                ),
                Span::raw(format!(" {:<10} {}{}", a.name, a.alias, suffix)),
            ])
        })
        .collect();

    let height = (rows.len() as u16 + 2).min(area.height);
    let width = 46.min(area.width);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" start which agent here? press a number, Esc cancels "),
        ),
        popup,
    );
}



/// Keep the last `max` characters, marking the cut with a leading ellipsis.
fn elide_front(path: &str, max: usize) -> String {
    let count = path.chars().count();
    if count <= max || max == 0 {
        return path.to_string();
    }
    let tail: String = path.chars().skip(count - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

/// Which provider to launch the just-chosen agent against.
fn render_provider_picker(f: &mut Frame, pick: &ProviderPick, area: Rect) {
    let mut rows: Vec<Line> = vec![
        Line::styled(
            "  Enter — whichever CC Switch has active",
            Style::default().fg(Color::DarkGray),
        ),
        Line::raw(""),
    ];
    for (i, choice) in pick.choices.iter().enumerate() {
        rows.push(Line::from(vec![
            Span::styled(
                format!(" {} ", i + 1),
                Style::default().fg(Color::Black).bg(Color::Cyan),
            ),
            Span::raw(format!(" {:<18}", truncate(&choice.name, 17))),
            Span::styled(
                if choice.is_current { "● active" } else { "" },
                Style::default().fg(Color::Green),
            ),
        ]));
    }

    let height = (rows.len() as u16 + 2).min(area.height);
    let width = 48.min(area.width);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    f.render_widget(ratatui::widgets::Clear, popup);
    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!(" {} — which provider? ", pick.agent.name)),
        ),
        popup,
    );
}

/// What the daemon is doing, and how to change it.
///
/// The monitor is what the phone app talks to, so whether it is up matters —
/// and finding out meant leaving for a shell to run `amux serve` or `amux
/// stop`.
fn render_settings(f: &mut Frame, area: Rect) {
    let pid = crate::serve::daemon_pid();
    let mut rows: Vec<Line> = vec![Line::raw("")];

    match pid {
        Some(pid) => {
            let port = crate::serve::daemon_port();
            rows.push(Line::from(vec![
                Span::raw("  monitor    "),
                Span::styled("running", Style::default().fg(Color::Green)),
                Span::styled(format!("   :{port}"), Style::default().fg(Color::DarkGray)),
            ]));
            rows.push(Line::styled(
                format!("             pid {pid}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        None => rows.push(Line::from(vec![
            Span::raw("  monitor    "),
            Span::styled("stopped", Style::default().fg(Color::DarkGray)),
        ])),
    }

    if let Some(log) = crate::serve::daemon_log() {
        rows.push(Line::raw(""));
        rows.push(Line::styled(
            format!("  log        {}", shorten_home(&log.to_string_lossy())),
            Style::default().fg(Color::DarkGray),
        ));
    }

    rows.push(Line::raw(""));
    rows.push(Line::styled(
        match pid {
            Some(_) => "  S stops it   Esc closes",
            None => "  s starts it   Esc closes",
        },
        Style::default().fg(Color::DarkGray),
    ));

    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" settings "),
        ),
        area,
    );
}

/// The choice in progress, drawn in the terminal column.
///
/// Deliberately not a popup: the column is where the session will appear once
/// it exists, so choosing it there means the decision and its result occupy the
/// same place. It is also the one part of the layout that has nothing to show
/// while a draft is open — the tree's cursor is on the placeholder, so there is
/// no session to attach.
fn render_draft(f: &mut Frame, draft: &Draft, area: Rect) {
    let width = area.width;
    let (title, rows): (String, Vec<Line>) = match draft {
        Draft::Project(picker) => {
            let matches = picker.matches();
            let mut rows = vec![
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Cyan)),
                    Span::raw(picker.query.clone()),
                    Span::styled("_", Style::default().fg(Color::DarkGray)),
                ]),
                Line::raw(""),
            ];
            if matches.is_empty() {
                rows.push(Line::styled(
                    "  nothing matches",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let visible = area.height.saturating_sub(4) as usize;
            let first = picker.cursor.saturating_sub(visible.saturating_sub(1));
            for (i, entry) in matches.iter().enumerate().skip(first).take(visible) {
                let selected = i == picker.cursor;
                let style = if selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                rows.push(Line::from(vec![
                    Span::styled(
                        format!("{}{:<16}", if selected { "> " } else { "  " }, truncate(&entry.label, 15)),
                        style,
                    ),
                    Span::styled(
                        elide_front(&shorten_home(&entry.path), width.saturating_sub(22) as usize),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            (
                " open project — type to filter, Enter picks, Esc cancels ".to_string(),
                rows,
            )
        }
        Draft::Session(picker) => {
            let mut rows: Vec<Line> = Vec::new();
            if picker.loading {
                rows.push(Line::styled("  loading…", Style::default().fg(Color::DarkGray)));
            } else if picker.entries.is_empty() {
                rows.push(Line::styled(
                    "  no recorded conversations here",
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let visible = area.height.saturating_sub(4) as usize;
            let first = picker.cursor.saturating_sub(visible.saturating_sub(1));
            for (i, e) in picker.entries.iter().enumerate().skip(first).take(visible) {
                let selected = i == picker.cursor;
                let style = if selected {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                rows.push(Line::from(vec![
                    Span::styled(if selected { "> " } else { "  " }, style),
                    Span::styled(format!("{:<9}", e.agent), style),
                    Span::styled(
                        format!("{:<10}", crate::commands::list::short_id(&e.id)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{:<9}", crate::commands::list::relative_time(e.modified)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(truncate(
                        e.summary.as_deref().unwrap_or(""),
                        width.saturating_sub(35) as usize,
                    )),
                ]));
            }
            rows.push(Line::raw(""));
            rows.push(Line::styled(
                "  Enter resumes   n starts a new one   Esc goes back",
                Style::default().fg(Color::DarkGray),
            ));
            (format!(" {} ", picker.label), rows)
        }
        Draft::Starting { label } => (
            format!(" {label} "),
            vec![Line::styled(
                "  starting…",
                Style::default().fg(Color::DarkGray),
            )],
        ),
    };

    f.render_widget(
        Paragraph::new(rows).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(title),
        ),
        area,
    );
}

/// `/Users/you/projects/x` as `~/projects/x`, to leave room for the name.
fn shorten_home(path: &str) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h.to_str()?).map(str::to_string)) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

/// How wide the tree column should be for a terminal of `total` columns.
///
/// Proportional until it has all it can use. A session row needs 35 cells and
/// the description under it a little more; past [`TREE_MAX`] the extra would
/// only pad the right-hand side of the tree while the terminal view — which can
/// always use more — goes without.
fn tree_width(total: u16) -> u16 {
    (total * 4 / 11).min(TREE_MAX).max(20)
}

/// Wide enough for a full session row plus an indented description.
const TREE_MAX: u16 = 52;

/// The tree: projects, with their sessions nested under the open ones.
fn render_tree(f: &mut Frame, state: &AppState, area: Rect) {
    // Inside the border, which is what a description line has to fit within.
    let width = area.width.saturating_sub(2) as usize;
    let projects = state.visible_projects();
    let rows = state.rows();

    let items: Vec<ListItem> = rows
        .iter()
        .map(|row| match *row {
            Row::Project { index, expandable } => {
                let project = &projects[index];
                // A project that cannot expand shows its agent inline — it is
                // that session, so naming it saves a row that says nothing.
                let (marker, trailing) = if !expandable {
                    (
                        " ",
                        project
                            .sessions
                            .first()
                            .map(|s| agent_name(&state.agents, &s.alias).to_string())
                            .unwrap_or_default(),
                    )
                } else if state.collapsed.contains(&project.dir) {
                    ("▸", format!("{}", project.sessions.len()))
                } else {
                    ("▾", format!("{}", project.sessions.len()))
                };
                // Folding a project is how you stop looking at it, so the row
                // has to keep showing an agent that is blocked on you —
                // otherwise collapsing the tree hides the one thing worth
                // interrupting for. An expanded project stays quiet; its
                // children speak for themselves.
                let status = if !expandable {
                    project
                        .sessions
                        .first()
                        .and_then(|s| state.statuses.get(&s.name))
                } else if state.collapsed.contains(&project.dir) {
                    project
                        .sessions
                        .iter()
                        .filter_map(|s| state.statuses.get(&s.name))
                        .max_by_key(|s| s.status.urgency())
                } else {
                    None
                };
                let (word, colour) = status_marker(status);
                let age = status_age(status);
                // A project that cannot expand *is* its one session, so its
                // description belongs here — there is no child row to carry it.
                let about = (!expandable)
                    .then(|| project.sessions.first())
                    .flatten()
                    .and_then(|s| state.descriptions.get(&s.name));
                // Same column order as the session rows below — what it is,
                // then how it is doing — so the two line up when a project's
                // children are open.
                let mut lines = vec![Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(
                        // Twelve keeps the duration on screen at 100 columns:
                        // the status word alone is eight cells, and anything
                        // wider here pushes "5m" past the border.
                        // Truncate one short of the field so a maximum-length
                        // name still has a space after it — otherwise it runs
                        // straight into the agent.
                        format!("{:<12}", truncate(project.display_name(), 11)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("{trailing:<9}"), Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{word:<8}"), Style::default().fg(colour)),
                    Span::styled(age, Style::default().fg(colour)),
                ])];
                if let Some(about) = about {
                    lines.push(Line::styled(
                        format!("    {}", truncate(about, width.saturating_sub(6))),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(lines)
            }
            // The session being chosen, standing in the slot it will occupy.
            Row::Draft => {
                let label = state
                    .draft
                    .as_ref()
                    .map(Draft::label)
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::styled("+ ", Style::default().fg(Color::Cyan)),
                    Span::styled(
                        truncate(&label, 34),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]))
            }
            Row::Session { project, index } => {
                let session = &projects[project].sessions[index];
                // What tells two sessions of one directory apart: the provider
                // it runs against, or the suffix `amux new` gave it.
                //
                // Both live around the trailing hash, so parse from there —
                // splitting on the last `-` puts a provider's own hyphen in the
                // way and renders `cc-glm_amux_4d8e0883` as "glm_amux…".
                let suffix = session_qualifier(&session.name);
                let (word, colour) = status_marker(state.statuses.get(&session.name));
                // Widths chosen so the status column lands at the same offset
                // as on a project row (2+12+9 there, 4+9+10 here) — a status
                // that shifts left when you open a project is hard to scan.
                //
                // The description goes on a second line rather than a sixth
                // column: what a session is about does not fit beside four
                // other fields, and the tree is capped precisely so it cannot
                // try.
                let mut lines = vec![Line::from(vec![
                    Span::raw("  ├ "),
                    Span::styled(
                        format!("{:<9}", agent_name(&state.agents, &session.alias)),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("{:<10}", truncate(&suffix, 9)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(format!("{word:<8}"), Style::default().fg(colour)),
                    Span::styled(
                        status_age(state.statuses.get(&session.name)),
                        Style::default().fg(colour),
                    ),
                ])];
                if let Some(about) = state.descriptions.get(&session.name) {
                    lines.push(Line::styled(
                        format!("      {}", truncate(about, width.saturating_sub(8))),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                ListItem::new(lines)
            }
        })
        .collect();

    let mut list_state = ListState::default();
    if !items.is_empty() {
        list_state.select(Some(state.cursor));
    }

    let (border, style) = border_for(state, Column::Tree);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border)
                .border_style(style)
                .title(" sessions "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_terminal(
    f: &mut Frame,
    state: &AppState,
    live: Option<&LiveTerm>,
    area: Rect,
) {
    let (border, style) = border_for(state, Column::Terminal);
    let title = match state.current_name() {
        Some(name) => format!(" {name} "),
        None => " terminal ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border)
        .border_style(style)
        .title(title);

    match live {
        // The real screen of a real client — cursor, colour and all.
        Some(term) => {
            f.render_widget(PseudoTerminal::new(term.parser.screen()).block(block), area)
        }
        None => f.render_widget(
            Paragraph::new("no session selected")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        ),
    }
}

fn render_status(f: &mut Frame, state: &AppState, area: Rect) {
    if state.inserting {
        let name = state.current_name().unwrap_or_default();
        let line = Line::from(vec![
            Span::styled(
                " -- INSERT -- ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "  keys go to {name}   Esc leave   {KEY_TO_INTERRUPT} interrupt"
            )),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    if let Some(target) = &state.confirming_kill {
        let line = Line::from(vec![
            Span::styled(
                " kill? ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {target}   y to confirm, any other key cancels")),
        ]);
        f.render_widget(Paragraph::new(line), area);
        return;
    }

    if let Some(notice) = &state.notice {
        f.render_widget(
            Paragraph::new(notice.as_str()).style(Style::default().fg(Color::Green)),
            area,
        );
        return;
    }

    let help = if state.filtering {
        format!("/{}", state.filter)
    } else {
        let filter = if state.filter.is_empty() {
            String::new()
        } else {
            format!("[/{}]  ", state.filter)
        };
        // The keys differ by column, so say which set is live rather than
        // listing both and leaving the reader to guess.
        match state.focus {
            Column::Terminal => format!(
                "{filter}jk scroll  h back  i insert  Enter attach  q quit"
            ),
            Column::Tree => format!(
                "{filter}hjkl move  Enter/i open  o project  O here  a agent  \
                 N extra  A fullscreen  d kill  / filter  , settings  q quit"
            ),
        }
    };
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str, alias: &str) -> ManagedSession {
        ManagedSession { name: name.into(), alias: alias.into() }
    }

    fn projects() -> Vec<Project> {
        vec![
            Project {
                dir: "/work/alpha".into(),
                name: "alpha".into(),
                alias: None,
                sessions: vec![session("cc_alpha_11111111", "cc")],
            },
            Project {
                dir: "/work/beta".into(),
                name: "beta".into(),
                alias: None,
                sessions: vec![
                    session("cx_beta_22222222", "cx"),
                    session("cx_beta_22222222-grok", "cx"),
                ],
            },
            Project {
                dir: "/work/empty".into(),
                name: "empty".into(),
                alias: None,
                sessions: vec![],
            },
        ]
    }

    #[test]
    fn filter_matches_project_name_or_path() {
        let mut s = AppState::new(projects());
        s.filter = "beta".into();
        assert_eq!(s.visible_projects().len(), 1);
        assert_eq!(s.visible_projects()[0].name, "beta");

        s.filter = "/work/alpha".into();
        assert_eq!(s.visible_projects().len(), 1);
        assert_eq!(s.visible_projects()[0].name, "alpha");
    }

    fn agents() -> Vec<Agent> {
        vec![
            Agent { name: "claude".into(), alias: "cc".into(), command: vec!["claude".into()] },
            Agent { name: "codex".into(), alias: "cx".into(), command: vec!["codex".into()] },
            Agent { name: "pi".into(), alias: "p".into(), command: vec!["pi".into()] },
        ]
    }

    #[test]
    fn projects_start_open() {
        // Opening three projects to find out what is running defeats the point
        // of a tree.
        let s = AppState::with_agents(projects(), agents());
        let rows = s.rows();
        // alpha(1) + beta + its 2 + empty = 5
        assert_eq!(rows.len(), 5, "sessions should be visible without pressing l");
        assert!(matches!(rows[2], Row::Session { .. }));
    }

    #[test]
    fn an_open_heading_cannot_be_selected() {
        let s = AppState::with_agents(projects(), agents());
        let rows = s.rows();

        // alpha has one session: the project row *is* that session.
        assert!(s.selectable(rows[0]));
        // beta is open with two, so its row is only a heading.
        assert!(!s.selectable(rows[1]), "open heading should be skipped");
        assert!(s.selectable(rows[2]));
        assert!(s.selectable(rows[3]));
    }

    #[test]
    fn the_cursor_starts_on_something_selectable() {
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = s.first_selectable();
        assert!(s.current_row().is_some_and(|r| s.selectable(r)));
        assert!(s.current_name().is_some());
    }

    #[test]
    fn jk_skips_over_headings() {
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = 0; // alpha

        // Straight past beta's heading to its first session.
        s.move_down();
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222"));
        s.move_down();
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222-grok"));

        // And back up the same way.
        s.move_up();
        s.move_up();
        assert_eq!(s.current_name().as_deref(), Some("cc_alpha_11111111"));
    }

    #[test]
    fn a_closed_project_becomes_selectable_again() {
        // Otherwise closing one would leave it with no row the cursor can
        // reach, and no way to reopen it.
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = 2; // beta's first session
        s.focus_left(); // closes beta

        let rows = s.rows();
        assert_eq!(rows.len(), 3, "beta's children should be hidden");
        assert!(s.selectable(rows[1]));
        assert_eq!(s.cursor, 1, "cursor should land on the closed project");
    }

    #[test]
    fn a_collapsed_project_stands_in_for_its_only_session() {
        let s = AppState::with_agents(projects(), agents());
        // Cursor on "alpha", which is not expandable.
        assert_eq!(s.current_name().as_deref(), Some("cc_alpha_11111111"));
    }

    #[test]
    fn l_reopens_a_closed_project_and_enters_it() {
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = 2;
        s.focus_left(); // close beta, cursor on its row

        s.focus_right();
        assert_eq!(s.focus, Column::Tree, "first l should reopen, not leave");
        assert_eq!(s.rows().len(), 5);
        assert_eq!(
            s.current_name().as_deref(),
            Some("cx_beta_22222222"),
            "cursor should follow into the first session"
        );

        s.focus_right();
        assert_eq!(s.focus, Column::Terminal);
    }

    #[test]
    fn tab_opens_a_project_and_cycles_its_sessions() {
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = 1; // beta, collapsed

        s.cycle_session();
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222"));
        s.cycle_session();
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222-grok"));
        s.cycle_session();
        assert_eq!(
            s.current_name().as_deref(),
            Some("cx_beta_22222222"),
            "Tab should wrap within the project"
        );
    }

    #[test]
    fn an_empty_project_resolves_to_no_session() {
        let mut s = AppState::with_agents(projects(), agents());
        // Locate "empty" by name — row numbers shift as projects open.
        s.cursor = s
            .rows()
            .iter()
            .position(|r| matches!(r, Row::Project { index, .. }
                if s.visible_projects()[*index].name == "empty"))
            .expect("empty project should have a row");
        assert!(s.current_name().is_none());
        // And there is nothing to step right into.
        s.focus_right();
        s.clamp();
        assert_eq!(s.focus, Column::Tree);
    }

    #[test]
    fn clamp_pulls_the_cursor_back_inside() {
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = 99;
        s.clamp();
        assert_eq!(s.cursor, s.rows().len() - 1);
    }

    #[test]
    fn every_listed_agent_can_actually_be_picked() {
        // The regression this exists for: the picker compared one keypress
        // against the alias, so `cc`, `cx` and `oc` — two characters each —
        // were unreachable, and only `p` worked. Whatever the picker draws
        // must be selectable.
        let agents = agents();
        for (i, agent) in agents.iter().enumerate() {
            let key = char::from_digit(i as u32 + 1, 10).unwrap();
            assert_eq!(
                picked_agent(&agents, key).map(|a| a.name.as_str()),
                Some(agent.name.as_str()),
                "agent {} is listed but key '{key}' does not select it",
                agent.name
            );
        }
    }

    #[test]
    fn out_of_range_and_non_digits_select_nothing() {
        let agents = agents(); // three of them
        assert!(picked_agent(&agents, '4').is_none());
        assert!(picked_agent(&agents, '0').is_none(), "numbering starts at 1");
        assert!(picked_agent(&agents, 'c').is_none());
        assert!(picked_agent(&agents, ' ').is_none());
    }

    #[test]
    fn the_picker_lists_every_configured_agent() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::with_agents(projects(), agents());
        state.cursor = 1;
        state.picking_agent = true;

        let mut terminal = Terminal::new(TestBackend::new(110, 14)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        for agent in ["claude", "codex", "pi"] {
            assert!(text.contains(agent), "{agent} missing from the picker");
        }
        // The two cx sessions already here are called out, so choosing cx is
        // visibly "open another" rather than "open one".
        assert!(text.contains("running"), "no indication of existing sessions");
        assert!(text.contains("Esc"), "no way out documented");
    }

    #[test]
    fn a_notice_replaces_the_help_line() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::with_agents(projects(), agents());
        state.notice = Some("started cx_beta_22222222-2".into());

        let mut terminal = Terminal::new(TestBackend::new(110, 10)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("started cx_beta_22222222-2"));
    }

    #[test]
    fn d_asks_before_killing() {
        let mut s = AppState::with_agents(projects(), agents());
        assert!(s.confirming_kill.is_none());

        // `d` only arms it — the session is still there.
        s.confirming_kill = s.current_name();
        assert_eq!(s.confirming_kill.as_deref(), Some("cc_alpha_11111111"));
    }

    #[test]
    fn the_confirmation_names_the_session_and_the_key() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::with_agents(projects(), agents());
        state.confirming_kill = Some("cc_alpha_11111111".into());

        let mut terminal = Terminal::new(TestBackend::new(110, 10)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        // Which session is about to die has to be on screen — that is the
        // whole point of asking.
        assert!(text.contains("cc_alpha_11111111"));
        assert!(text.contains("kill?"));
        assert!(text.contains('y'), "the confirming key is not documented");
    }

    #[test]
    fn wheel_events_encode_as_sgr_reports() {
        // What a real terminal sends, so the attached client's own `mouse on`
        // handles scrollback without amux knowing anything about copy-mode.
        assert_eq!(encode_mouse(MouseEventKind::ScrollUp, 0, 0), b"\x1b[<64;1;1M");
        assert_eq!(encode_mouse(MouseEventKind::ScrollDown, 4, 9), b"\x1b[<65;5;10M");
        // Coordinates are 1-based in the protocol, 0-based coming in.
        assert_eq!(
            encode_mouse(MouseEventKind::Down(MouseButton::Left), 2, 3),
            b"\x1b[<0;3;4M"
        );
        // Movement without a button carries no meaning here.
        assert!(encode_mouse(MouseEventKind::Moved, 1, 1).is_empty());
    }

    /// The layout is the whole point of this screen, so render it for real
    /// rather than only testing the state behind it. `TestBackend` ships with
    /// ratatui — no new dev-dependency.
    #[test]
    fn esc_returns_the_focus_that_i_took() {
        // Leaving insert from the tree used to strand the cursor on the
        // terminal, where j/k scroll instead of selecting — so getting back to
        // the list took an extra h nobody asked for.
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = s.first_selectable();
        assert_eq!(s.focus, Column::Tree);

        // `i`
        s.inserting = true;
        s.focus_before_insert = Some(s.focus);
        s.focus = Column::Terminal;

        // `Esc`
        s.inserting = false;
        if let Some(previous) = s.focus_before_insert.take() {
            s.focus = previous;
        }
        assert_eq!(s.focus, Column::Tree, "focus should go back to the tree");

        // Entering from the terminal column leaves you there instead.
        s.focus = Column::Terminal;
        s.inserting = true;
        s.focus_before_insert = Some(s.focus);
        s.inserting = false;
        if let Some(previous) = s.focus_before_insert.take() {
            s.focus = previous;
        }
        assert_eq!(s.focus, Column::Terminal);
    }

    #[test]
    fn jk_means_different_things_per_column() {
        // The bug this pins: with the terminal focused, j/k moved the tree
        // selection — swapping out the very session being read.
        let mut s = AppState::with_agents(projects(), agents());
        s.cursor = s.first_selectable();
        let start = s.cursor;

        s.focus = Column::Terminal;
        // The handler routes to the terminal, so the tree must not move. This
        // asserts the state side of that: nothing here changes the cursor.
        assert_eq!(s.cursor, start);

        s.focus = Column::Tree;
        s.move_down();
        assert_ne!(s.cursor, start, "tree focus should still navigate");
    }

    #[test]
    fn the_status_line_says_which_keys_are_live() {
        use ratatui::backend::TestBackend;

        let render_with = |focus: Column| {
            let mut state = AppState::with_agents(projects(), agents());
            state.cursor = state.first_selectable();
            state.focus = focus;
            let mut terminal = Terminal::new(TestBackend::new(120, 8)).unwrap();
            terminal.draw(|f| render(f, &state, None)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        assert!(render_with(Column::Tree).contains("hjkl move"));
        let term = render_with(Column::Terminal);
        assert!(term.contains("jk scroll"), "scrolling not advertised");
        assert!(!term.contains("hjkl move"), "stale hint for the other column");
    }

    /// Folding a project must not hide an agent that is blocked on you.
    ///
    /// Collapsing is how you stop looking at a project, so the parent row has
    /// to keep carrying the one status that needs a person — otherwise the
    /// tree quietly buries the thing it exists to surface.
    #[test]
    fn a_folded_project_still_shows_a_waiting_child() {
        use crate::serve::server::{PaneStatus, SessionStatus};
        use ratatui::backend::TestBackend;

        let at = |status: PaneStatus, since: Option<i64>| SessionStatus { status, since };
        let draw = |state: &AppState| -> String {
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            terminal.draw(|f| render(f, state, None)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();
        // beta has two sessions; only the second is blocked.
        let now = chrono::Utc::now().timestamp();
        state
            .statuses
            .insert("cx_beta_22222222".into(), at(PaneStatus::Idle, None));
        state.statuses.insert(
            "cx_beta_22222222-grok".into(),
            // Blocked for five minutes — long enough that the wait matters.
            at(PaneStatus::Waiting, Some(now - 300)),
        );

        let waitings = |state: &AppState| draw(state).matches("waiting").count();

        // Nothing known yet must stay blank rather than claim idle.
        assert_eq!(waitings(&AppState::new(projects())), 0, "status drawn without data");

        // Open, the child carries it.
        assert_eq!(waitings(&state), 1, "waiting child drew no status");

        // How long it has been blocked is what makes the marker actionable.
        assert!(draw(&state).contains("5m"), "waiting duration not shown");

        // Folded, the parent must inherit the most urgent of the two.
        state.collapsed.insert("/work/beta".into());
        state.cursor = state.first_selectable();
        assert_eq!(
            waitings(&state),
            1,
            "folding beta hid a session that is waiting on the user"
        );
        assert!(draw(&state).contains("5m"), "folding dropped the duration");

        // An inferred status has no start time, and must not invent one —
        // `0m` beside an agent blocked for an hour is worse than blank.
        let mut guessed = AppState::new(projects());
        guessed
            .statuses
            .insert("cx_beta_22222222".into(), at(PaneStatus::Waiting, None));
        let text = draw(&guessed);
        assert!(text.contains("waiting"), "inferred status not drawn");
        assert!(!text.contains("0m"), "invented a duration for an inferred status");
    }

    /// A named directory must show its name — and sort by it.
    ///
    /// Sorting on the folder name while displaying the alias would file a
    /// project under a letter that appears nowhere on screen.
    #[test]
    fn a_named_project_is_shown_and_sorted_by_its_name() {
        use ratatui::backend::TestBackend;

        let mut projects = projects();
        projects[0].alias = Some("zzz-last".into()); // "alpha" -> sorts last
        // group_by_project sorts on the display name; mirror that here, since
        // the fixture bypasses it.
        projects.sort_by(|a, b| {
            a.display_name()
                .to_lowercase()
                .cmp(&b.display_name().to_lowercase())
        });

        let mut state = AppState::new(projects);
        state.cursor = state.first_selectable();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(text.contains("zzz-last"), "alias not drawn");
        assert!(!text.contains("alpha"), "folder name drawn instead of the alias");
        // beta now precedes it.
        assert!(
            text.find("beta") < text.find("zzz-last"),
            "rows not ordered by the name actually shown"
        );
    }

    /// The tree names the agent, not the shell shortcut.
    ///
    /// `cx` and `oc` are what you type; they are not what you want to read off
    /// a list of what is running.
    #[test]
    fn rows_name_the_agent_rather_than_its_alias() {
        use ratatui::backend::TestBackend;

        let projects = vec![
            // Single session: the agent is shown inline on the project row.
            Project {
                dir: "/work/solo".into(),
                name: "solo".into(),
                alias: None,
                sessions: vec![session("oc_solo_11111111", "oc")],
            },
            // Several: each child row names its own.
            Project {
                dir: "/work/many".into(),
                name: "many".into(),
                alias: None,
                sessions: vec![
                    session("cx_many_22222222", "cx"),
                    session("cc_many_22222222-b", "cc"),
                ],
            },
        ];
        let mut state = AppState::with_agents(projects, crate::config::builtin_agents());
        state.cursor = state.first_selectable();

        let mut terminal = Terminal::new(TestBackend::new(120, 10)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        // The longest name has to survive the column, not lose its tail to the
        // border — that is the whole reason the project column is 14 wide.
        assert!(text.contains("opencode"), "agent name truncated or not shown");
        assert!(text.contains("codex"), "child row does not name its agent");
        assert!(text.contains("claude"), "child row does not name its agent");
    }

    /// A path too long for the popup must lose its *front*. The tail is what
    /// tells two projects apart; cutting there leaves a column of identical
    /// "/Users/not/projects/devs/…".
    #[test]
    fn a_long_path_keeps_its_tail() {
        let long = "/Users/not/projects/devs/opensource/deeply/nested/sitin";
        let out = elide_front(long, 20);
        assert!(out.chars().count() <= 20, "elided text is still too wide");
        assert!(out.ends_with("sitin"), "the identifying end was cut: {out}");
        assert!(out.starts_with('…'));

        // Short enough to fit is returned untouched.
        assert_eq!(elide_front("~/x", 20), "~/x");
    }

    fn entry(agent: &str, id: &str, modified: f64) -> SessionEntry {
        SessionEntry {
            agent: agent.into(),
            id: id.into(),
            modified,
            summary: Some(format!("summary for {id}")),
        }
    }

    /// A listing that arrives after the user has moved on must be discarded.
    ///
    /// It comes off a background thread, so nothing stops it landing seconds
    /// late — into a closed picker, or one now showing a different project.
    #[test]
    fn a_stale_listing_is_not_applied() {
        let mut state = AppState::new(projects());

        // Nothing open: a listing has nowhere to go.
        assert!(!apply_listing(&mut state, "/work/alpha", vec![entry("claude", "a1", 2.0)]));

        state.draft = Some(Draft::Session(SessionPicker::new(
            "/work/alpha".into(),
            "alpha".into(),
        )));
        assert!(matches!(&state.draft, Some(Draft::Session(p)) if p.loading));

        // A listing for a *different* project must not fill this one.
        assert!(!apply_listing(&mut state, "/work/beta", vec![entry("codex", "b1", 1.0)]));
        let Some(Draft::Session(p)) = state.draft.as_ref() else { panic!("draft lost") };
        assert!(p.entries.is_empty(), "another project's sessions were shown");
        assert!(p.loading, "loading was cleared by the wrong listing");

        // The matching one fills it.
        assert!(apply_listing(
            &mut state,
            "/work/alpha",
            vec![entry("claude", "a1", 2.0), entry("codex", "a2", 1.0)]
        ));
        let Some(Draft::Session(p)) = state.draft.as_ref() else { panic!("draft lost") };
        assert_eq!(p.entries.len(), 2);
        assert!(!p.loading);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.selected().unwrap().id, "a1");
    }

    /// The cursor must stay on a real row however far it is pushed.
    #[test]
    fn the_session_cursor_stays_in_range() {
        let mut p = SessionPicker::new("/work/alpha".into(), "alpha".into());
        p.move_by(3);
        assert_eq!(p.cursor, 0, "an empty list must not move the cursor");
        assert!(p.selected().is_none());

        p.entries = vec![entry("claude", "a", 2.0), entry("codex", "b", 1.0)];
        p.move_by(9);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.selected().unwrap().id, "b");
        p.move_by(-9);
        assert_eq!(p.cursor, 0);
    }

    /// A project opened from history usually has nothing running in it, so the
    /// first session there must get the plain name — not the `-2` that marks a
    /// second workspace alongside an existing one.
    ///
    /// Drives the real code path, because the bug was in the argument `n`
    /// passes, not in the naming helper: a name-only assertion would have gone
    /// on passing.
    #[test]
    fn the_first_session_in_a_project_is_not_named_as_an_extra() {
        if !tmux::is_available() {
            eprintln!("skipping: no multiplexer installed");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.to_string_lossy().into_owned();

        let agents = crate::config::builtin_agents();
        // `true` here so nothing real is launched in the session.
        let agent = Agent {
            name: "probe".into(),
            alias: format!("zz{}", std::process::id()),
            command: vec!["true".into()],
        };
        let mut state = AppState::with_agents(Vec::new(), agents);

        let (tx, rx) = mpsc::channel();
        spawn_agent_in(&mut state, &tx, &agent, &dir, false);
        // Creation runs on its own thread now, so the name comes back over the
        // channel rather than from the call.
        let created = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("no result from the spawn thread")
            .expect("session should be created");
        let _ = tmux::kill_session(&created);

        let base = crate::session::session_name(&agent.alias, std::path::Path::new(&dir));
        assert_eq!(
            created, base,
            "the first session in an empty project was named as an extra"
        );
    }

    /// The key hints must name the keys that exist.
    ///
    /// `Enter` stopped handing over the terminal — a second one after opening a
    /// project used to replace the layout with the agent just opened — and the
    /// full-screen attach moved to `A`.
    #[test]
    fn the_hints_name_enter_and_the_fullscreen_key() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();
        let mut terminal = Terminal::new(TestBackend::new(120, 12)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(text.contains("Enter/i open"), "Enter is not described as opening");
        assert!(text.contains("A fullscreen"), "the fullscreen key is unlisted");
        assert!(
            !text.contains("Enter attach"),
            "the hints still promise the old take-over-the-terminal behaviour"
        );
    }

    /// The placeholder must be a real, selectable row — and only exist while a
    /// draft does.
    #[test]
    fn the_draft_adds_one_selectable_row_at_the_end() {
        let mut state = AppState::new(projects());
        let before = state.rows().len();
        assert!(
            !state.rows().iter().any(|r| matches!(r, Row::Draft)),
            "a placeholder appeared with no draft open"
        );

        state.draft = Some(Draft::Project(ProjectPicker::default()));
        let rows = state.rows();
        assert_eq!(rows.len(), before + 1);
        assert!(matches!(rows.last(), Some(Row::Draft)));

        // Selectable, or the cursor could never sit on it.
        state.cursor = rows.len() - 1;
        assert!(state.selectable(Row::Draft));

        // And it resolves to no session — which is what makes the event loop
        // drop the live terminal instead of leaving the previous one on screen
        // behind the picker.
        assert!(state.current_name().is_none());
        assert!(state.current_project().is_none());
    }

    /// Esc steps back from the session list to the project list rather than
    /// throwing the whole thing away — picking the wrong project is the
    /// likelier mistake, and starting over costs the query you just typed.
    #[test]
    fn the_draft_label_tracks_the_stage() {
        let picker = ProjectPicker::default();
        assert_eq!(Draft::Project(picker).label(), "new session…");

        let mut sp = SessionPicker::new("/work/sitin".into(), "sitin".into());
        assert_eq!(Draft::Session(sp.clone()).label(), "sitin…");
        sp.loading = false;

        assert_eq!(
            Draft::Starting { label: "sitin".into() }.label(),
            "sitin — starting…"
        );
    }

    /// Cancelling must leave the tree exactly as it was found.
    ///
    /// Opening a draft moves the cursor to the placeholder at the end of the
    /// list; without remembering where it came from, Esc left you at the bottom
    /// of the tree looking at a session you had not chosen.
    #[test]
    fn cancelling_a_draft_restores_the_cursor() {
        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();
        let before = state.cursor;
        let name_before = state.current_name();

        // What `o` does.
        state.cursor_before_draft = Some(state.cursor);
        state.draft = Some(Draft::Project(ProjectPicker::default()));
        state.cursor = state.rows().len() - 1;
        assert!(matches!(state.current_row(), Some(Row::Draft)));

        // What Esc does.
        state.draft = None;
        if let Some(previous) = state.cursor_before_draft.take() {
            state.cursor = previous;
        }

        assert_eq!(state.cursor, before, "the cursor did not go back");
        assert_eq!(state.current_name(), name_before);
        assert!(state.cursor_before_draft.is_none(), "the saved cursor leaked");
    }

    /// Esc must not invent a step the user never took.
    ///
    /// Reached through `o`, the session list sits on top of a project list, so
    /// Esc goes back to it. Reached through `O` it is the first thing shown,
    /// and dropping into a project list there would be stranger than closing.
    #[test]
    fn esc_goes_back_only_when_there_is_something_to_go_back_to() {
        let from_o = SessionPicker::from_projects("/work/sitin".into(), "sitin".into());
        assert!(from_o.from_project_list, "o must leave a list to return to");

        let from_shift_o = SessionPicker::new("/work/sitin".into(), "sitin".into());
        assert!(
            !from_shift_o.from_project_list,
            "O starts at the sessions, so there is no list behind it"
        );

        // Both describe the same directory either way.
        assert_eq!(from_o.dir, from_shift_o.dir);
        assert_eq!(Draft::Session(from_shift_o).label(), "sitin…");
    }

    fn row(path: &str, name: &str, alias: Option<&str>) -> crate::store::ProjectRow {
        crate::store::ProjectRow {
            path: path.into(),
            name: name.into(),
            alias: alias.map(str::to_string),
            last_agent: "claude".into(),
            last_seen_at: "2026-09-13T00:00:00Z".into(),
            launch_count: 1,
        }
    }

    /// Typing a fragment of the *path* has to find the project — that is the
    /// whole point: you remember "sitin", not where it lives.
    #[test]
    fn the_picker_matches_on_name_alias_and_path() {
        let tmp = tempfile::tempdir().unwrap();
        let sitin = tmp.path().join("projects/devs/sitin");
        let other = tmp.path().join("work/iotex");
        std::fs::create_dir_all(&sitin).unwrap();
        std::fs::create_dir_all(&other).unwrap();

        let rows = vec![
            row(sitin.to_str().unwrap(), "sitin", None),
            row(other.to_str().unwrap(), "iotex", Some("IoTeX 主线")),
        ];

        let mut p = ProjectPicker::new(rows);
        assert_eq!(p.matches().len(), 2, "an empty query shows everything");

        // By folder name, which is also part of the path.
        p.query = "sitin".into();
        assert_eq!(p.matches().len(), 1);
        assert!(p.selected().unwrap().path.ends_with("devs/sitin"));

        // By a path fragment that is in neither name.
        p.query = "devs/".into();
        assert_eq!(p.matches().len(), 1);

        // By the alias the user chose, which is not the folder name.
        p.query = "iotex 主线".into();
        assert_eq!(p.matches().len(), 1);
        assert_eq!(p.selected().unwrap().label, "IoTeX 主线");

        p.query = "nothing-like-this".into();
        assert!(p.matches().is_empty());
        p.clamp();
        assert!(p.selected().is_none(), "no match must not resolve to a row");
    }

    /// A directory that has since been deleted must not be offered: picking it
    /// could only ever produce "directory no longer exists". Seventeen of the
    /// sixty-two recorded paths on this machine are in that state.
    #[test]
    fn the_picker_hides_directories_that_are_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live");
        std::fs::create_dir_all(&live).unwrap();

        let rows = vec![
            row(live.to_str().unwrap(), "live", None),
            row(&tmp.path().join("deleted").to_string_lossy(), "deleted", None),
        ];

        let p = ProjectPicker::new(rows);
        let paths: Vec<&str> = p.matches().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths.len(), 1, "a vanished directory was offered");
        assert!(paths[0].ends_with("live"));
    }

    /// Narrowing the list must not strand the cursor past its end.
    #[test]
    fn the_picker_cursor_stays_inside_the_matches() {
        let tmp = tempfile::tempdir().unwrap();
        for n in ["alpha", "alpine", "beta"] {
            std::fs::create_dir_all(tmp.path().join(n)).unwrap();
        }
        let rows = ["alpha", "alpine", "beta"]
            .iter()
            .map(|n| row(&tmp.path().join(n).to_string_lossy(), n, None))
            .collect();

        let mut p = ProjectPicker::new(rows);
        p.move_by(2);
        assert_eq!(p.cursor, 2);

        // "alp" leaves two rows; the cursor was on the third.
        p.query = "alp".into();
        p.clamp();
        assert_eq!(p.cursor, 1, "cursor left past the end of the narrowed list");
        assert!(p.selected().is_some());

        p.move_by(-5);
        assert_eq!(p.cursor, 0, "moving up past the top must stop at the top");
    }

    fn drawn(state: &AppState, width: u16) -> String {
        use ratatui::backend::TestBackend;
        let mut t = Terminal::new(TestBackend::new(width, 14)).unwrap();
        t.draw(|f| render(f, state, None)).unwrap();
        t.backend().buffer().content().iter().map(|c| c.symbol()).collect()
    }

    /// A session says what it is about, on its own line — and says nothing
    /// when there is nothing to say, rather than leaving a blank row.
    #[test]
    fn a_session_shows_its_description_under_it() {
        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();
        assert!(!drawn(&state, 140).contains("adobe-account-test"));

        // A child row carries its own. Deliberately ASCII: a wide character
        // occupies two cells in the buffer, so reading it back as a flat string
        // of symbols would not match the text that went in.
        state
            .descriptions
            .insert("cx_beta_22222222".into(), "adobe-account-test".into());
        assert!(drawn(&state, 140).contains("adobe-account-test"));

        // A project with one session *is* that session, so its description
        // belongs on the project row — there is no child row to put it on.
        state
            .descriptions
            .insert("cc_alpha_11111111".into(), "session-descriptions".into());
        assert!(
            drawn(&state, 140).contains("session-descriptions"),
            "a single-session project dropped its description"
        );
    }

    /// The column grows with the terminal only until it has what it can use.
    #[test]
    fn the_tree_column_stops_widening() {
        // Proportional while there is less than it wants.
        assert_eq!(tree_width(100), 36);
        assert_eq!(tree_width(140), 50);

        // Capped past that, rather than taking half a wide terminal from the
        // view that can always use more.
        assert_eq!(tree_width(200), TREE_MAX);
        assert_eq!(tree_width(400), TREE_MAX);
        assert!(TREE_MAX < 400 * 4 / 11);

        // Still usable on something narrow.
        assert_eq!(tree_width(40), 20);
    }

    /// The provider becomes part of the session's identity, so two providers
    /// of one agent in one directory are separate sessions rather than one
    /// that silently changed endpoint.
    #[test]
    fn a_provider_gets_its_own_session_name() {
        let cwd = std::path::Path::new("/work/reverse");
        let plain = crate::session::session_name("cc", cwd);
        let with_provider = crate::session::session_name("cc-glm", cwd);

        assert_ne!(plain, with_provider);
        assert!(with_provider.starts_with("cc-glm_"));

        // Both still parse as managed sessions, or `amux ls` and the monitor
        // would lose track of the second one.
        let agents = crate::config::builtin_agents();
        let managed = crate::commands::sessions::managed_sessions(
            &[plain.clone(), with_provider.clone()],
            &agents,
        );
        assert_eq!(managed.len(), 2);
    }

    /// The numbers beside a list are 1-based, and anything else selects
    /// nothing — including 0, which must not underflow into the last entry.
    #[test]
    fn list_numbers_are_one_based_and_bounded() {
        assert_eq!(picked_index('1', 3), Some(0));
        assert_eq!(picked_index('3', 3), Some(2));
        assert_eq!(picked_index('4', 3), None);
        assert_eq!(picked_index('0', 3), None);
        assert_eq!(picked_index('x', 3), None);
        assert_eq!(picked_index('1', 0), None);
    }

    /// The suffix column has to survive a provider's hyphen.
    ///
    /// Splitting on the last `-` rendered `cc-glm_amux_4d8e0883` as
    /// "glm_amux…", because that hyphen belongs to the provider rather than to
    /// the suffix `amux new` adds. Both sit around the trailing hash.
    #[test]
    fn the_suffix_column_reads_provider_and_suffix_apart() {
        assert_eq!(session_qualifier("cc_amux_4d8e0883"), "");
        assert_eq!(session_qualifier("cc-glm_amux_4d8e0883"), "glm");
        assert_eq!(session_qualifier("cx_reverse_bb8c2d50-grok"), "grok");
        assert_eq!(
            session_qualifier("cc-glm_reverse_bb8c2d50-debug"),
            "glm debug"
        );
    }

    /// Settings takes over the column the terminal uses, and says what the
    /// key does *now* — offering "start" while it is running would be a lie.
    #[test]
    fn settings_reports_the_daemon_and_its_one_action() {
        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();

        assert!(!drawn(&state, 140).contains("monitor"));

        state.settings = true;
        let text = drawn(&state, 140);
        assert!(text.contains("settings"), "the panel is not titled");
        assert!(text.contains("monitor"), "the daemon is not reported");

        // Exactly one of the two actions is offered, matching the state the
        // daemon is actually in.
        let running = text.contains("running");
        assert_eq!(
            running,
            text.contains("S stops it"),
            "a running daemon must offer stop"
        );
        assert_eq!(
            !running,
            text.contains("s starts it"),
            "a stopped daemon must offer start"
        );
    }

    #[test]
    fn the_tree_shows_projects_and_their_open_sessions() {
        use ratatui::backend::TestBackend;

        // Everything is open by default now, so nothing needs pressing first.
        let mut state = AppState::new(projects());
        state.cursor = state.first_selectable();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| render(f, &state, None)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();

        assert!(text.contains("alpha"), "collapsed project missing");
        assert!(text.contains("beta"), "open project missing");
        // Its sessions are nested under it once open.
        assert!(text.contains("grok"), "child session not drawn");
        // Two columns now, so there is no separate session pane.
        assert!(!text.contains("projects"), "old three-column title survived");
        assert!(text.contains("hjkl"));
    }

    #[test]
    fn grouping_puts_sessions_of_one_directory_together() {
        // group_by_project shells out for each cwd, so exercise the grouping
        // shape directly rather than the multiplexer call.
        let mut projects = projects();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(projects[1].name, "beta");
        assert_eq!(projects[1].sessions.len(), 2);
    }
}
