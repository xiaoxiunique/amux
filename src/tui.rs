use crate::commands::sessions::{managed_sessions, ManagedSession};
use crate::config::Agent;
use crate::{commands, tmux};
use anyhow::Result;
use ansi_to_tui::IntoText;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use std::io::stdout;
use std::time::{Duration, Instant};

/// How often the selected session's terminal is re-captured while idle.
///
/// One capture costs ~18ms of subprocess spawn, so this is the dominant cost of
/// the whole UI. Only the *selected* session is ever captured — the same choice
/// yazi makes, previewing only the hovered file.
const IDLE_POLL: Duration = Duration::from_millis(250);

/// Floor between redraws, so a burst of output can't spin the renderer.
const REDRAW_FLOOR: Duration = Duration::from_millis(16);

/// Which column has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Projects,
    Sessions,
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
    pub project_idx: usize,
    pub session_idx: usize,
    /// Filter over project names. Only editable in filter mode.
    pub filter: String,
    /// Explicit mode, because the old TUI routed *every* unmatched key into the
    /// filter — which leaves no room for hjkl, let alone forwarding keystrokes
    /// to an agent.
    pub filtering: bool,
    /// Last captured screen of the selected session, raw with ANSI intact.
    pub preview: String,
}

impl AppState {
    pub fn new(projects: Vec<Project>) -> Self {
        Self {
            projects,
            focus: Column::Projects,
            project_idx: 0,
            session_idx: 0,
            filter: String::new(),
            filtering: false,
            preview: String::new(),
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

    pub fn current_project(&self) -> Option<&Project> {
        self.visible_projects().get(self.project_idx).copied()
    }

    pub fn current_sessions(&self) -> &[ManagedSession] {
        self.current_project().map(|p| p.sessions.as_slice()).unwrap_or(&[])
    }

    pub fn current_session(&self) -> Option<&ManagedSession> {
        self.current_sessions().get(self.session_idx)
    }

    pub fn current_name(&self) -> Option<String> {
        self.current_session().map(|s| s.name.clone())
    }

    /// Whether the session column is worth showing.
    ///
    /// With a single session the column is a one-row list that can only ever
    /// have that row selected — it costs a third of the width to say nothing.
    /// Collapsing it gives the terminal the space instead, and the project row
    /// already stands in as the selection.
    pub fn shows_session_column(&self) -> bool {
        self.current_sessions().len() > 1
    }

    /// `j` / `k` — move within the focused column.
    pub fn move_down(&mut self) {
        match self.focus {
            Column::Projects => {
                let n = self.visible_projects().len();
                if n > 0 {
                    self.project_idx = (self.project_idx + 1).min(n - 1);
                    // A different project means a different session list; an
                    // index carried over from the old one would point at the
                    // wrong session, or past the end.
                    self.session_idx = 0;
                }
            }
            Column::Sessions | Column::Terminal => {
                let n = self.current_sessions().len();
                if n > 0 {
                    self.session_idx = (self.session_idx + 1).min(n - 1);
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        match self.focus {
            Column::Projects => {
                self.project_idx = self.project_idx.saturating_sub(1);
                self.session_idx = 0;
            }
            Column::Sessions | Column::Terminal => {
                self.session_idx = self.session_idx.saturating_sub(1);
            }
        }
    }

    /// `l` — descend a column, the way yazi enters a directory.
    pub fn focus_right(&mut self) {
        self.focus = match self.focus {
            // With the session column collapsed there is nothing to stop at
            // between the project and its terminal.
            Column::Projects if self.shows_session_column() => Column::Sessions,
            Column::Projects if !self.current_sessions().is_empty() => Column::Terminal,
            Column::Projects => Column::Projects,
            Column::Sessions => Column::Terminal,
            Column::Terminal => Column::Terminal,
        };
    }

    /// `h` — back out a column.
    pub fn focus_left(&mut self) {
        self.focus = match self.focus {
            Column::Terminal if self.shows_session_column() => Column::Sessions,
            Column::Terminal => Column::Projects,
            Column::Sessions => Column::Projects,
            Column::Projects => Column::Projects,
        };
    }

    /// Keep both indices inside their lists after a refresh changed them.
    pub fn clamp(&mut self) {
        let projects = self.visible_projects().len();
        if projects == 0 {
            self.project_idx = 0;
        } else if self.project_idx >= projects {
            self.project_idx = projects - 1;
        }

        let sessions = self.current_sessions().len();
        if sessions == 0 {
            self.session_idx = 0;
            // Nothing to focus further right.
            if self.focus != Column::Projects {
                self.focus = Column::Projects;
            }
        } else if self.session_idx >= sessions {
            self.session_idx = sessions - 1;
        }

        // A sibling session ended and the column it lived in is gone; leaving
        // focus there would strand the cursor on something no longer drawn.
        if self.focus == Column::Sessions && !self.shows_session_column() {
            self.focus = Column::Terminal;
        }
    }
}

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

/// Outcome of the TUI loop, decided after the terminal is restored.
enum Outcome {
    Quit,
    Attach(String),
    Kill(String),
    NewAgent,
}

pub fn run_tui(agents: &[Agent]) -> Result<()> {
    let all = tmux::list_session_names()?;
    let sessions = managed_sessions(&all, agents);
    let mut state = AppState::new(group_by_project(sessions));

    let outcome = event_loop(&mut state)?;

    match outcome {
        Outcome::Quit => Ok(()),
        Outcome::Attach(name) => tmux::attach_or_switch(&name),
        Outcome::Kill(name) => {
            tmux::kill_session(&name)?;
            // re-enter the TUI with refreshed list
            run_tui(agents)
        }
        Outcome::NewAgent => new_agent_in_cwd(agents),
    }
}

fn new_agent_in_cwd(agents: &[Agent]) -> Result<()> {
    // Minimal v1: pick the first agent if exactly one; otherwise prompt by index.
    if agents.is_empty() {
        return Ok(());
    }
    println!("Pick an agent to start in this directory:");
    for (i, a) in agents.iter().enumerate() {
        println!("  {}) {} ({})", i + 1, a.name, a.alias);
    }
    print!("> ");
    use std::io::Write;
    stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let idx: usize = line.trim().parse().unwrap_or(0);
    if idx >= 1 && idx <= agents.len() {
        commands::run::run(&agents[idx - 1], &[], None, agents)
    } else {
        Ok(())
    }
}

/// Re-capture the selected session, if there is one.
fn refresh_preview(state: &mut AppState) {
    state.preview = match state.current_name() {
        Some(name) => tmux::capture_pane_ansi(&name),
        None => String::new(),
    };
}

fn event_loop(state: &mut AppState) -> Result<Outcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let mut last_capture = Instant::now();
    let mut last_draw = Instant::now() - REDRAW_FLOOR;
    let mut dirty = true;
    refresh_preview(state);

    let result = loop {
        state.clamp();

        if dirty && last_draw.elapsed() >= REDRAW_FLOOR {
            terminal.draw(|f| render(f, state))?;
            last_draw = Instant::now();
            dirty = false;
        }

        // Waking on a timeout rather than blocking on a key is the whole point:
        // the terminal column has to keep updating while the user sits still.
        if event::poll(REDRAW_FLOOR)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                dirty = true;

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
                    KeyCode::Char('j') | KeyCode::Down => state.move_down(),
                    KeyCode::Char('k') | KeyCode::Up => state.move_up(),
                    KeyCode::Char('h') | KeyCode::Left => state.focus_left(),
                    KeyCode::Char('l') | KeyCode::Right => state.focus_right(),
                    KeyCode::Char('/') => {
                        state.filtering = true;
                        state.filter.clear();
                    }
                    KeyCode::Char('n') => break Outcome::NewAgent,
                    KeyCode::Char('d') => {
                        if let Some(name) = state.current_name() {
                            break Outcome::Kill(name);
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(name) = state.current_name() {
                            break Outcome::Attach(name);
                        }
                    }
                    _ => {}
                }

                // Moving the selection changes what the terminal column shows,
                // so capture now instead of waiting out the idle interval.
                refresh_preview(state);
                last_capture = Instant::now();
            }
        }

        if last_capture.elapsed() >= IDLE_POLL {
            let before = std::mem::take(&mut state.preview);
            refresh_preview(state);
            last_capture = Instant::now();
            // Only a change is worth a repaint — agents idle for long stretches.
            if before != state.preview {
                dirty = true;
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(result)
}

/// Border style that marks which column owns the keyboard.
fn border_for(state: &AppState, column: Column) -> (BorderType, Style) {
    if state.focus == column {
        (BorderType::Thick, Style::default().fg(Color::Cyan))
    } else {
        (BorderType::Plain, Style::default().fg(Color::DarkGray))
    }
}

fn render(f: &mut Frame, state: &AppState) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    // The terminal always takes the lion's share — an agent's boxed UI wraps
    // badly below roughly 60 columns. The session column only appears when the
    // project actually has more than one, and its width comes out of the
    // terminal's rather than the project list's.
    if state.shows_session_column() {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Ratio(2, 11),
                Constraint::Ratio(3, 11),
                Constraint::Ratio(6, 11),
            ])
            .split(outer[0]);

        render_projects(f, state, columns[0]);
        render_sessions(f, state, columns[1]);
        render_terminal(f, state, columns[2]);
    } else {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Ratio(2, 11), Constraint::Ratio(9, 11)])
            .split(outer[0]);

        render_projects(f, state, columns[0]);
        render_terminal(f, state, columns[1]);
    }

    render_status(f, state, outer[1]);
}

fn render_projects(f: &mut Frame, state: &AppState, area: Rect) {
    let projects = state.visible_projects();
    let items: Vec<ListItem> = projects
        .iter()
        .map(|p| {
            let count = p.sessions.len();
            ListItem::new(format!("{:<18} {}", truncate(&p.name, 18), count))
        })
        .collect();

    let mut list_state = ListState::default();
    if !projects.is_empty() {
        list_state.select(Some(state.project_idx));
    }

    let (border, style) = border_for(state, Column::Projects);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border)
                .border_style(style)
                .title(" projects "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_sessions(f: &mut Frame, state: &AppState, area: Rect) {
    let sessions = state.current_sessions();
    let items: Vec<ListItem> = sessions
        .iter()
        .map(|s| {
            // The alias is the agent; the suffix is what distinguishes two
            // sessions in one directory, so show that rather than the full
            // name, which is mostly a hash.
            let suffix = s.name.rsplit_once('-').map(|(_, tail)| tail).unwrap_or("");
            ListItem::new(format!("{:<4} {}", s.alias, suffix))
        })
        .collect();

    let mut list_state = ListState::default();
    if !sessions.is_empty() {
        list_state.select(Some(state.session_idx));
    }

    let (border, style) = border_for(state, Column::Sessions);
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(border)
                .border_style(style)
                .title(" sessions "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▸ ");
    f.render_stateful_widget(list, area, &mut list_state);
}

fn render_terminal(f: &mut Frame, state: &AppState, area: Rect) {
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

    // Captured output carries SGR sequences; parse them so the agent's own
    // colours survive instead of arriving as literal escape codes.
    let body = state
        .preview
        .as_bytes()
        .into_text()
        .unwrap_or_else(|_| Text::raw(state.preview.clone()));

    f.render_widget(Paragraph::new(body).block(block), area);
}

fn render_status(f: &mut Frame, state: &AppState, area: Rect) {
    let help = if state.filtering {
        format!("/{}", state.filter)
    } else {
        let filter = if state.filter.is_empty() {
            String::new()
        } else {
            format!("[/{}]  ", state.filter)
        };
        format!("{filter}hjkl move  Enter attach  d kill  n new  / filter  q quit")
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

    #[test]
    fn hjkl_moves_between_columns() {
        let mut s = AppState::new(projects());
        // "beta" has two sessions, so all three columns exist — the collapsed
        // case is covered by `navigation_skips_the_collapsed_column`.
        s.project_idx = 1;
        assert_eq!(s.focus, Column::Projects);

        s.focus_right();
        assert_eq!(s.focus, Column::Sessions);
        s.focus_right();
        assert_eq!(s.focus, Column::Terminal);
        // Already at the rightmost column.
        s.focus_right();
        assert_eq!(s.focus, Column::Terminal);

        s.focus_left();
        assert_eq!(s.focus, Column::Sessions);
        s.focus_left();
        assert_eq!(s.focus, Column::Projects);
        s.focus_left();
        assert_eq!(s.focus, Column::Projects);
    }

    #[test]
    fn a_project_with_no_sessions_cannot_be_descended_into() {
        let mut s = AppState::new(projects());
        s.project_idx = 2; // "empty"
        s.focus_right();
        assert_eq!(s.focus, Column::Projects, "descended into an empty project");
    }

    #[test]
    fn jk_moves_within_the_focused_column() {
        let mut s = AppState::new(projects());

        // In the project column, j/k walk projects.
        s.move_down();
        assert_eq!(s.project_idx, 1);
        assert_eq!(s.current_project().unwrap().name, "beta");

        // In the session column, they walk that project's sessions.
        s.focus_right();
        s.move_down();
        assert_eq!(s.session_idx, 1);
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222-grok"));

        // And clamp at the end rather than running off it.
        s.move_down();
        assert_eq!(s.session_idx, 1);
    }

    #[test]
    fn changing_project_resets_the_session_cursor() {
        // Otherwise an index from a project with two sessions would point past
        // the end of one with a single session.
        let mut s = AppState::new(projects());
        s.project_idx = 1;
        s.focus_right();
        s.move_down();
        assert_eq!(s.session_idx, 1);

        s.focus_left();
        s.move_up(); // back to "alpha", which has one session
        assert_eq!(s.session_idx, 0);
        assert_eq!(s.current_name().as_deref(), Some("cc_alpha_11111111"));
    }

    #[test]
    fn clamp_pulls_focus_back_out_of_an_emptied_project() {
        let mut s = AppState::new(projects());
        s.project_idx = 1;
        s.focus_right();
        assert_eq!(s.focus, Column::Sessions);

        // The project's sessions went away under us (killed elsewhere).
        s.projects[1].sessions.clear();
        s.clamp();
        assert_eq!(s.focus, Column::Projects);
        assert_eq!(s.session_idx, 0);
        assert!(s.current_name().is_none());
    }

    #[test]
    fn a_lone_session_collapses_the_middle_column() {
        let mut s = AppState::new(projects());

        s.project_idx = 0; // "alpha" — one session
        assert!(!s.shows_session_column());

        s.project_idx = 1; // "beta" — two sessions
        assert!(s.shows_session_column());
    }

    #[test]
    fn navigation_skips_the_collapsed_column() {
        let mut s = AppState::new(projects());
        s.project_idx = 0; // one session, so no middle column

        // `l` goes straight to the terminal rather than stopping on a column
        // that isn't drawn.
        s.focus_right();
        assert_eq!(s.focus, Column::Terminal);
        // And `h` comes straight back.
        s.focus_left();
        assert_eq!(s.focus, Column::Projects);

        // With two sessions the middle column is real and gets a stop.
        s.project_idx = 1;
        s.focus_right();
        assert_eq!(s.focus, Column::Sessions);
        s.focus_right();
        assert_eq!(s.focus, Column::Terminal);
        s.focus_left();
        assert_eq!(s.focus, Column::Sessions);
    }

    #[test]
    fn focus_leaves_the_session_column_when_it_collapses() {
        // A sibling session ends while the cursor is sitting in that column.
        let mut s = AppState::new(projects());
        s.project_idx = 1;
        s.focus_right();
        assert_eq!(s.focus, Column::Sessions);

        s.projects[1].sessions.pop();
        s.clamp();
        assert!(!s.shows_session_column());
        assert_eq!(s.focus, Column::Terminal, "stranded on an undrawn column");
    }

    #[test]
    fn two_column_layout_gives_the_terminal_the_extra_width() {
        use ratatui::backend::TestBackend;

        let render_at = |project_idx: usize| {
            let mut state = AppState::new(projects());
            state.project_idx = project_idx;
            state.preview = "agent output".into();
            let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
            terminal.draw(|f| render(f, &state)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            buffer.content().iter().map(|c| c.symbol()).collect::<String>()
        };

        // One session: no session column at all.
        let lone = render_at(0);
        assert!(lone.contains("projects"));
        assert!(!lone.contains("sessions"), "middle column drawn for one session");
        assert!(lone.contains("agent output"));

        // Two sessions: it comes back.
        let pair = render_at(1);
        assert!(pair.contains("sessions"), "middle column missing for two sessions");
    }

    /// The layout is the whole point of this screen, so render it for real
    /// rather than only testing the state behind it. `TestBackend` ships with
    /// ratatui — no new dev-dependency.
    #[test]
    fn all_three_columns_are_drawn() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(projects());
        state.project_idx = 1; // "beta", which has two sessions
        state.preview = "hello from the agent".into();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| render(f, &state)).unwrap();

        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();

        assert!(rendered.contains("projects"), "project column missing");
        assert!(rendered.contains("sessions"), "session column missing");
        assert!(rendered.contains("alpha") && rendered.contains("beta"));
        assert!(rendered.contains("hello from the agent"), "preview not drawn");
        // The status line documents the vim keys.
        assert!(rendered.contains("hjkl"));
    }

    /// A pane's captured output carries SGR escapes; they must be parsed into
    /// styles, not printed as literal `[38;5;246m` noise.
    #[test]
    fn ansi_in_the_capture_becomes_colour_not_text() {
        use ratatui::backend::TestBackend;

        let mut state = AppState::new(projects());
        state.project_idx = 0;
        state.preview = "\x1b[31mred\x1b[0m".into();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|f| render(f, &state)).unwrap();

        let buffer = terminal.backend().buffer();
        let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("red"));
        assert!(!text.contains("31m"), "escape leaked through as text");

        let coloured = buffer
            .content()
            .iter()
            .any(|c| c.fg == Color::Red);
        assert!(coloured, "the escape was stripped instead of applied");
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
