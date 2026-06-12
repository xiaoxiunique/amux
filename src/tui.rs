use crate::commands::sessions::{managed_sessions, ManagedSession};
use crate::config::Agent;
use crate::{commands, tmux};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::io::stdout;

/// Pure UI state, independent of rendering.
pub struct AppState {
    pub sessions: Vec<ManagedSession>,
    pub filter: String,
    pub selected: usize,
}

impl AppState {
    pub fn new(sessions: Vec<ManagedSession>) -> Self {
        Self { sessions, filter: String::new(), selected: 0 }
    }

    /// Sessions whose name contains the filter (case-insensitive).
    pub fn visible(&self) -> Vec<&ManagedSession> {
        let f = self.filter.to_lowercase();
        self.sessions
            .iter()
            .filter(|s| f.is_empty() || s.name.to_lowercase().contains(&f))
            .collect()
    }

    pub fn move_down(&mut self) {
        let n = self.visible().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn clamp(&mut self) {
        let n = self.visible().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// The currently selected session name, if any.
    pub fn current_name(&self) -> Option<String> {
        self.visible().get(self.selected).map(|s| s.name.clone())
    }
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
    let mut state = AppState::new(sessions);

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
        commands::run::run(&agents[idx - 1], &[], None)
    } else {
        Ok(())
    }
}

fn event_loop(state: &mut AppState) -> Result<Outcome> {
    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = loop {
        state.clamp();
        terminal.draw(|f| render(f, state))?;

        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break Outcome::Quit,
                KeyCode::Down | KeyCode::Char('j') => state.move_down(),
                KeyCode::Up | KeyCode::Char('k') => state.move_up(),
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
                KeyCode::Backspace => {
                    state.filter.pop();
                }
                KeyCode::Char(c) => state.filter.push(c),
                _ => {}
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(result)
}

fn render(f: &mut Frame, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(f.area());

    let visible = state.visible();
    let items: Vec<ListItem> = visible
        .iter()
        .map(|s| ListItem::new(format!("{:<28} {}", s.name, s.alias)))
        .collect();

    let mut list_state = ListState::default();
    if !visible.is_empty() {
        list_state.select(Some(state.selected));
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" amux sessions "))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    let help = format!(
        "filter: {}    Enter attach  d kill  n new  / type to filter  q quit",
        state.filter
    );
    f.render_widget(
        Paragraph::new(help).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessions() -> Vec<ManagedSession> {
        vec![
            ManagedSession { name: "cc_alpha_11111111".into(), alias: "cc".into() },
            ManagedSession { name: "cx_beta_22222222".into(), alias: "cx".into() },
            ManagedSession { name: "cc_gamma_33333333".into(), alias: "cc".into() },
        ]
    }

    #[test]
    fn filter_matches_substring() {
        let mut s = AppState::new(sessions());
        s.filter = "beta".into();
        assert_eq!(s.visible().len(), 1);
        assert_eq!(s.visible()[0].name, "cx_beta_22222222");
    }

    #[test]
    fn movement_clamps_to_visible() {
        let mut s = AppState::new(sessions());
        s.move_up(); // already at 0
        assert_eq!(s.selected, 0);
        s.move_down();
        s.move_down();
        s.move_down(); // clamp at last (index 2)
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn current_name_follows_selection() {
        let mut s = AppState::new(sessions());
        s.move_down();
        assert_eq!(s.current_name().as_deref(), Some("cx_beta_22222222"));
    }
}
