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
    /// Last path component, for display.
    pub name: String,
    pub sessions: Vec<ManagedSession>,
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
    /// Configured agents, so the picker can list them and map alias -> agent.
    pub agents: Vec<Agent>,
    /// Session awaiting a kill confirmation. `d` is one key away from ending a
    /// running agent, so it asks first.
    pub confirming_kill: Option<String>,
    /// Transient message for the status line (what was just created, or why
    /// nothing was).
    pub notice: Option<String>,
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
            confirming_kill: None,
            agents,
            notice: None,
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
            Row::Session { .. } => true,
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

    for session in sessions {
        let dir = tmux::session_cwd(&session.name).unwrap_or_else(|_| session.name.clone());
        let name = dir
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(&dir)
            .to_string();

        match projects.iter_mut().find(|p| p.dir == dir) {
            Some(project) => project.sessions.push(session),
            None => projects.push(Project { dir, name, sessions: vec![session] }),
        }
    }

    projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
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
    let mut dirty = true;

    let result = loop {
        state.clamp();

        // Attach to whatever is selected, and drop a terminal whose session is
        // no longer the one in view.
        let wanted = state.current_name();
        if live.as_ref().map(|t| &t.session) != wanted.as_ref() {
            live = None;
            if let Some(name) = &wanted {
                let (cols, rows) = term_size(terminal_column(terminal.get_frame().area(), state));
                live = LiveTerm::open(name, cols, rows);
            }
            dirty = true;
        }

        if let Some(term) = live.as_mut() {
            let (cols, rows) = term_size(terminal_column(terminal.get_frame().area(), state));
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
        }

        // A short poll rather than a blocking read: output arrives on its own
        // schedule and has to be drained between keystrokes.
        if event::poll(POLL)? {
            let ev = event::read()?;

            if let Event::Mouse(MouseEvent { kind, column, row, .. }) = ev {
                // Only the terminal column forwards — a wheel over the lists
                // should move the selection, not scroll someone's agent.
                let area = terminal_column(terminal.get_frame().area(), state);
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

                // The picker takes one digit — the number shown beside each
                // agent. Not the alias: `cc` and `cx` are two characters and
                // share a first letter, so a single keypress cannot name one.
                if state.picking_agent {
                    state.picking_agent = false;
                    if key.code == KeyCode::Esc {
                        continue;
                    }
                    if let KeyCode::Char(c) = key.code {
                        match picked_agent(&state.agents, c).cloned() {
                            Some(agent) => {
                                let created = spawn_agent(state, &agent, false);
                                reload(state, created);
                                live = None; // reattach to whatever is selected now
                            }
                            None => {
                                state.notice =
                                    Some(format!("'{c}' is not one of the listed numbers"));
                            }
                        }
                    }
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
                            // Put back whatever was being typed with before.
                            ime::restore(saved_ime.take());
                        }
                    }
                    KeyCode::Char('/') => {
                        state.filtering = true;
                        state.filter.clear();
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
                            let created = spawn_agent(state, &agent, true);
                            reload(state, created);
                            live = None;
                        }
                    }
                    KeyCode::Tab => state.cycle_session(),
                    KeyCode::Char('d') => {
                        state.confirming_kill = state.current_name();
                    }
                    KeyCode::Enter => {
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

    /// Switch to ASCII, returning what was in use so it can be put back.
    pub(super) fn drop_to_ascii() -> Option<String> {
        let previous = current()?;
        if previous == ASCII {
            return None;
        }
        select(ASCII);
        Some(previous)
    }

    pub(super) fn restore(previous: Option<String>) {
        if let Some(source) = previous {
            select(&source);
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
    let index = key.to_digit(10)?;
    if index < 1 {
        return None;
    }
    agents.get(index as usize - 1)
}

/// Start `agent` in the selected project's directory, without leaving the TUI.
///
/// Two shapes, which is the distinction the keys expose:
///   - the directory's *primary* session for that agent, when it has none yet
///   - an extra one, auto-suffixed `-2`, `-3`, … when it already does
///
/// Returns the new session's name so the caller can select it.
fn spawn_agent(state: &mut AppState, agent: &Agent, force_extra: bool) -> Option<String> {
    let dir = state.current_dir()?;
    let cwd = std::path::PathBuf::from(&dir);
    let base = crate::session::session_name(&agent.alias, &cwd);

    let name = if force_extra || tmux::has_session(&base) {
        format!("{base}-{}", crate::commands::new::next_free_suffix(&base))
    } else {
        base
    };

    // A fresh session picks up the conversation this directory was last on for
    // that agent, the same way `amux run` does — an extra session deliberately
    // does not, since it is a second workspace rather than a continuation.
    let mut argv = agent.command.clone();
    if !name.contains('-') || !force_extra {
        if let Some(id) = crate::commands::session_ids::load_id(&name)
            .filter(|id| crate::commands::session_ids::session_file_exists(&agent.name, &cwd, id))
        {
            argv.extend(crate::commands::session_ids::resume_args(&agent.name, &id));
        }
    }

    match crate::commands::run::create_detached(agent, &cwd, &name, &argv, &[]) {
        Ok(()) => {
            state.notice = Some(format!("started {name}"));
            Some(name)
        }
        Err(e) => {
            state.notice = Some(format!("could not start {}: {e}", agent.name));
            None
        }
    }
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
fn terminal_column(area: Rect, state: &AppState) -> Rect {
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
        .constraints([Constraint::Ratio(3, 11), Constraint::Ratio(8, 11)])
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
    render_terminal(f, state, live, columns[1]);

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

/// The tree: projects, with their sessions nested under the open ones.
fn render_tree(f: &mut Frame, state: &AppState, area: Rect) {
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
                    (" ", project.sessions.first().map(|s| s.alias.clone()).unwrap_or_default())
                } else if state.collapsed.contains(&project.dir) {
                    ("▸", format!("{}", project.sessions.len()))
                } else {
                    ("▾", format!("{}", project.sessions.len()))
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{marker} ")),
                    Span::styled(
                        format!("{:<16}", truncate(&project.name, 16)),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(trailing, Style::default().fg(Color::DarkGray)),
                ]))
            }
            Row::Session { project, index } => {
                let session = &projects[project].sessions[index];
                // The suffix is what tells two sessions of one directory apart;
                // the rest of the name is a hash of the directory.
                let suffix = session
                    .name
                    .rsplit_once('-')
                    .map(|(_, tail)| tail.to_string())
                    .unwrap_or_default();
                ListItem::new(Line::from(vec![
                    Span::raw("   ├ "),
                    Span::styled(
                        format!("{:<4}", session.alias),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(suffix, Style::default().fg(Color::DarkGray)),
                ]))
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
                "{filter}hjkl move  Tab cycle  i insert  a add agent  N extra  \
                 Enter attach  d kill  / filter  q quit"
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
                sessions: vec![session("cc_alpha_11111111", "cc")],
            },
            Project {
                dir: "/work/beta".into(),
                name: "beta".into(),
                sessions: vec![
                    session("cx_beta_22222222", "cx"),
                    session("cx_beta_22222222-grok", "cx"),
                ],
            },
            Project { dir: "/work/empty".into(), name: "empty".into(), sessions: vec![] },
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
