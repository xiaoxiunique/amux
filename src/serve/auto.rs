//! Auto mode: keep a stopped agent going.
//!
//! The failure this exists for is mundane. An agent finishes a turn, or parks
//! at a question, and there is nobody at the keyboard to type "continue" — so
//! the work sits there until its owner notices. The daemon already knows the
//! pane went Idle/Waiting (`build_snapshot`) and already knows how to type into
//! it (`paste_text`). Auto mode adds the missing judgement: ask a model whether
//! the session's goal still has work left, and if so, *what* the next
//! instruction should be.
//!
//! The decision policy lives in the server (`auto_tick`, which owns the
//! cooldown, the in-flight guard, and the turn budget). This module is the part
//! worth testing on its own: the prompt, the JSON contract, and the parsing of
//! the model's reply.

use std::time::Duration;

/// What the deciding model told us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Keep going, with this exact text pasted into the pane.
    Continue(String),
    /// The model looked and said there is nothing left to do, or a human is
    /// needed. Disarms. Carries the model's own words, so the log says *why*.
    Stop(String),
    /// We could not get a usable answer — no key, timeout, HTTP error, or a
    /// reply that broke the contract. Distinct from `Stop` on purpose: a
    /// transient failure or a missing key must not silently turn auto off.
    Unavailable,
}

/// A continuation longer than this is a sign the model ignored the contract,
/// so it is discarded rather than pasted as a wall of text.
const MAX_MESSAGE_CHARS: usize = 2000;

/// The supervisor prompt used when no override file is present.
///
/// `~/.config/amux/auto-prompt.md`, when it has content, replaces this — see
/// [`system_prompt`]. Keep the two in step: the file starts as a copy of this.
pub(crate) const DEFAULT_SYSTEM_PROMPT: &str = "\
You supervise a coding agent that has stopped and is waiting for input.
You are given its goal, how many automatic continuations have already been sent,
whether it is idle at the end of a turn or blocked on a prompt, and the tail of
its terminal.

Decide whether the agent should be nudged to keep going.
Return only JSON: {\"continue\": true|false, \"message\": \"...\"}.

Rules:
- If the goal is a real goal: continue=true when there is still work toward it
  that the agent can do on its own; continue=false when it looks met, when there
  is nothing useful left to do, or when a human must decide something only the
  user can answer.
- If the goal is exactly (none), judge from the tail alone: continue=true
  whenever the work shown can plainly be carried on — including when the agent
  just proposed a next step, a TODO, or an open question it could itself pursue.
  continue=false only when the turn ends as a final deliverable with no stated
  follow-up, or when the agent is waiting on something only the user can give
  (a credential, a decision, a physical action).
- When continue=true, put the next step in \"message\" as a direct instruction to
  the agent, written in the same language as the terminal tail above. Write it
  the way a person would type it: specific, brief, no preamble.
- When the agent is blocked on a permission or confirmation prompt, continue
  only if that action is plainly safe or authorized; otherwise continue=false.
- Never ask the agent to repeat work the tail shows it already finished.";

/// The supervisor prompt to send: the override file when it has content, else
/// the built-in default.
///
/// Read fresh on every decision, so editing `~/.config/amux/auto-prompt.md`
/// takes effect on the next stop — no rebuild, no daemon restart. An empty or
/// unreadable file falls back rather than sending an empty system message.
pub(crate) fn system_prompt() -> String {
    if let Some(path) = crate::config::auto_prompt_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    DEFAULT_SYSTEM_PROMPT.to_string()
}

/// What the goal line reads when the user armed the default "keep going" mode
/// instead of naming a goal. The system prompt keys off this exact string, so
/// the two must not drift.
pub(crate) const NO_GOAL: &str = "(none)";

/// Continuations a fresh auto run gets before it disarms itself.
///
/// A safety budget, not a target: it bounds the model calls and the agent work
/// when nobody is watching. Ten turned out to be small enough to strand real
/// tasks mid-way, so the default is generous and the form can still raise or
/// lower it.
pub(crate) const DEFAULT_MAX_TURNS: u32 = 50;

/// The user turn: everything the model gets to reason over.
pub(crate) fn build_prompt(
    goal: &str,
    tail: &str,
    used: u32,
    max_turns: u32,
    waiting: bool,
) -> String {
    let state = if waiting {
        "blocked on a prompt (possibly a permission/confirmation question)"
    } else {
        "idle at the end of a turn"
    };
    let goal = if goal.trim().is_empty() {
        NO_GOAL
    } else {
        goal
    };
    format!(
        "GOAL:\n{goal}\n\nCONTINUATIONS SENT: {used} of {max_turns}\nSTATE: {state}\n\n\
         RECENT TERMINAL OUTPUT (tail):\n{tail}"
    )
}

/// Parse the model's reply.
///
/// A reply we cannot turn into an instruction is `Unavailable`, not `Stop`:
/// the model did not say the work is done, so auto should stay armed and ask
/// again rather than disarm on a bad turn.
pub(crate) fn parse_decision(raw: &str) -> Decision {
    let trimmed = raw.trim();
    // `response_format: json_object` should already give bare JSON, but a model
    // that wraps it in prose or a fence should not cost us the answer.
    let candidate = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if end > start => &trimmed[start..=end],
        _ => trimmed,
    };

    let Ok(value) = serde_json::from_str::<serde_json::Value>(candidate) else {
        return Decision::Unavailable;
    };
    // An explicit false is the model deciding the goal is done / a human is
    // needed — the one case that disarms. Keep its reason for the log.
    if value.get("continue").and_then(|v| v.as_bool()) == Some(false) {
        let reason = value
            .get("message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        return Decision::Stop(reason);
    }
    if value.get("continue").and_then(|v| v.as_bool()) != Some(true) {
        return Decision::Unavailable;
    }
    let message = value
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if message.is_empty() || message.chars().count() > MAX_MESSAGE_CHARS {
        return Decision::Unavailable;
    }
    Decision::Continue(message.to_string())
}

/// Which command speaks for the deciding model. Overridable so a machine
/// without `claude` on its PATH, or with a wrapper around it, can still run
/// auto.
pub(crate) fn model_cli() -> String {
    std::env::var("AMUX_AUTO_CLI").unwrap_or_else(|_| "claude".to_string())
}

/// Whether the deciding model can be reached at all.
///
/// Just "is the command there" — running it to find out would cost a turn on
/// every poll. A CLI that is present but unauthorised shows up later as a
/// failed decision, which leaves auto armed to try again.
pub(crate) fn model_available() -> bool {
    let cli = model_cli();
    if cli.contains('/') {
        return std::path::Path::new(&cli).is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| dir.join(&cli).is_file())
        })
        .unwrap_or(false)
}

/// The model auto thinks with.
fn model_name() -> String {
    std::env::var("AMUX_AUTO_MODEL").unwrap_or_else(|_| "claude-opus-4-6".to_string())
}

/// Ask the local Claude Code for one answer, with no session and no tools.
///
/// `None` on anything that stops an answer coming back — the CLI missing, a
/// non-zero exit, a timeout, output that is not the JSON envelope. Callers turn
/// that into "ask again later" rather than into a decision.
///
/// Two flags carry the cost of this. `--strict-mcp-config` drops the MCP tool
/// definitions and `--setting-sources ""` the settings and their memory files;
/// measured on this machine, a decision goes from $0.81 to $0.014 once the
/// prefix is warm. The prefix that gets cached is the CLI's own — 26.9k tokens,
/// byte-identical every call — while the goal and the terminal tail are not
/// cached at all, which is why caching cannot carry one session's context into
/// another's decision.
///
/// Run from a scratch directory on purpose: whatever amux happens to be sitting
/// in has nothing to do with the session being judged, and a project's own
/// instructions are not addressed to a supervisor.
fn ask(system: &str, user: &str) -> Option<String> {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    let child = Command::new(model_cli())
        .args(["--print", "--output-format", "json"])
        .args(["--model", &model_name()])
        .args(["--system-prompt", system])
        .arg("--strict-mcp-config")
        .args(["--setting-sources", ""])
        // Nothing here is ever resumed, and every decision would otherwise leave
        // a transcript under ~/.claude/projects keyed by the scratch directory —
        // 104 files and 5.3MB had accumulated on this machine before anyone
        // looked.
        .arg("--no-session-persistence")
        .arg(user)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| eprintln!("[auto] cannot run {}: {error}", model_cli()))
        .ok()?;

    // Collected on a thread so the wait cannot deadlock against a full pipe,
    // and so a model that never answers does not wedge the loop that asked.
    let (tx, rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(MODEL_TIMEOUT) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            eprintln!("[auto] {} failed: {error}", model_cli());
            return None;
        }
        Err(_) => {
            eprintln!("[auto] {} did not answer within {MODEL_TIMEOUT:?}", model_cli());
            return None;
        }
    };
    let _ = handle.join();

    if !output.status.success() {
        // Both streams: the CLI reports its own failures as JSON on stdout and
        // leaves stderr empty, so logging stderr alone says only "it failed".
        let tail = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes).trim().chars().take(300).collect::<String>()
        };
        eprintln!(
            "[auto] {} exited {} — stderr: {} — stdout: {}",
            model_cli(),
            output.status,
            tail(&output.stderr),
            tail(&output.stdout)
        );
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| eprintln!("[auto] {} returned invalid JSON: {error}", model_cli()))
        .ok()?;
    if value.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
        eprintln!("[auto] {} reported an error turn", model_cli());
        return None;
    }
    value
        .get("result")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// How long to wait for a decision. Measured at under three seconds with the
/// trimming flags on; this is the ceiling before the loop gives up and asks
/// again on the next stop.
pub(crate) const MODEL_TIMEOUT: Duration = Duration::from_secs(90);

/// Ask the deciding model.
///
/// Anything that stops us from getting an answer — the CLI missing, a timeout,
/// a bad reply — is `Unavailable`, so the caller can leave auto armed and try
/// again instead of disarming on a failure.
pub(crate) fn decide(goal: &str, tail: &str, used: u32, max_turns: u32, waiting: bool) -> Decision {
    let Some(reply) = ask(
        &system_prompt(),
        &build_prompt(goal, tail, used, max_turns, waiting),
    ) else {
        return Decision::Unavailable;
    };
    parse_decision(&reply)
}

/// How much of a session's output to show the labeller. A title needs the gist,
/// not the whole scrollback, and this is the one part that costs tokens.
const LABEL_CONTEXT_CHARS: usize = 4000;

/// A label longer than this is the model ignoring the contract; the session's
/// own name is a better fallback than a paragraph.
const MAX_LABEL_CHARS: usize = 24;

const LABEL_SYSTEM_PROMPT: &str = "\
You name coding sessions so their owner can tell them apart at a glance.
Given the recent output of one session, reply with only JSON:
{\"label\": \"...\"}.

Rules:
- The label is a short title — a few words, at most 16 characters.
- It says what the session is *working on*, not what it is (not \"coding agent\",
  not the directory or the agent's name).
- Write it in the same language as the output: Chinese output, Chinese label.
- No quotes, no trailing punctuation, no file paths, no line breaks.";

/// Ask the model for a short title for a session, from its recent output.
///
/// `None` on any failure — no key, timeout, or a reply that is not a usable
/// label — so the caller leaves the existing name alone rather than blanking it.
pub(crate) fn suggest_label(context: &str) -> Option<String> {
    let context = context.trim();
    if context.is_empty() {
        return None;
    }
    // Budget the tail: the newest output is what the session is doing now.
    let recent: String = context
        .chars()
        .rev()
        .take(LABEL_CONTEXT_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let reply = ask(LABEL_SYSTEM_PROMPT, &format!("RECENT OUTPUT:\n{recent}"))?;
    parse_label(&reply)
}

/// Pull a usable title out of the model's reply; anything malformed is `None`.
fn parse_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let candidate = match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if end > start => &trimmed[start..=end],
        _ => trimmed,
    };
    let value = serde_json::from_str::<serde_json::Value>(candidate).ok()?;
    let label = value
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .map(|s| s.trim_matches(['"', '「', '」', '“', '”']).trim())
        .unwrap_or("");
    if label.is_empty() {
        return None;
    }
    let label: String = label.chars().take(MAX_LABEL_CHARS).collect();
    Some(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A label reply becomes a short title; anything unusable is `None` so the
    /// session keeps the name it had.
    #[test]
    fn a_label_reply_becomes_a_short_title() {
        assert_eq!(
            parse_label(r#"{"label": "登录修复"}"#).as_deref(),
            Some("登录修复")
        );
        // A fenced reply, and a value that still carries its own quotes.
        assert_eq!(
            parse_label("```json\n{\"label\": \"\\\"xhs sig\\\"\"}\n```").as_deref(),
            Some("xhs sig")
        );
        assert_eq!(parse_label(r#"{"nope": 1}"#), None);
        assert_eq!(parse_label(r#"{"label": "   "}"#), None);
        assert_eq!(parse_label("sure!"), None);
        // An overlong title is cut, not rejected.
        let long = "x".repeat(MAX_LABEL_CHARS + 10);
        assert_eq!(
            parse_label(&format!(r#"{{"label": "{long}"}}"#)).map(|l| l.chars().count()),
            Some(MAX_LABEL_CHARS)
        );
    }

    #[test]
    fn a_continue_decision_carries_the_instruction() {
        let decision = parse_decision(
            r#"{"continue": true, "message": "Run the failing test and fix it."}"#,
        );
        assert_eq!(
            decision,
            Decision::Continue("Run the failing test and fix it.".to_string())
        );
    }

    /// Only an explicit `continue: false` stops auto. Everything else that we
    /// cannot turn into an instruction is `Unavailable`, so a bad turn leaves
    /// the goal armed rather than silently disarming it.
    #[test]
    fn only_an_explicit_stop_stops() {
        assert_eq!(
            parse_decision(r#"{"continue": false, "message": "goal met"}"#),
            Decision::Stop("goal met".to_string())
        );
        // A false with no reason still stops; the log just has less to say.
        assert_eq!(
            parse_decision(r#"{"continue": false}"#),
            Decision::Stop(String::new())
        );
        // Missing field.
        assert_eq!(parse_decision(r#"{"message": "keep going"}"#), Decision::Unavailable);
        // Empty instruction.
        assert_eq!(
            parse_decision(r#"{"continue": true, "message": "  "}"#),
            Decision::Unavailable
        );
        // Not JSON at all.
        assert_eq!(parse_decision("sure, keep going!"), Decision::Unavailable);
        // Overlong instruction.
        let long = "x".repeat(MAX_MESSAGE_CHARS + 1);
        assert_eq!(
            parse_decision(&format!(r#"{{"continue": true, "message": "{long}"}}"#)),
            Decision::Unavailable
        );
    }

    /// Models sometimes wrap the object in prose or a fenced block even when
    /// asked for JSON; the decision should still be read out of it.
    #[test]
    fn a_wrapped_json_object_is_still_parsed() {
        let decision = parse_decision(
            "Here you go:\n```json\n{\"continue\": true, \"message\": \"Continue with step 2.\"}\n```",
        );
        assert_eq!(decision, Decision::Continue("Continue with step 2.".to_string()));
    }

    #[test]
    fn the_prompt_carries_the_goal_budget_and_state() {
        let prompt = build_prompt("port the parser to Rust", "TAIL-MARKER", 2, 8, false);
        assert!(prompt.contains("port the parser to Rust"));
        assert!(prompt.contains("TAIL-MARKER"));
        assert!(prompt.contains("2 of 8"));
        assert!(prompt.contains("idle at the end of a turn"));

        let waiting = build_prompt("goal", "", 0, 5, true);
        assert!(waiting.contains("blocked on a prompt"));
    }

    /// A blank goal is the default "keep going" mode: the prompt carries the
    /// marker the system prompt keys off, so the model judges from the tail.
    #[test]
    fn a_blank_goal_asks_the_model_to_judge_from_the_tail() {
        let prompt = build_prompt("", "TAIL", 0, 5, false);
        assert!(prompt.contains(NO_GOAL), "the no-goal marker is missing");
        assert!(prompt.contains("TAIL"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("judge from the tail alone"));
    }

    /// The supervisor prompt comes from `auto-prompt.md` when it has content,
    /// and from the built-in default when it is missing or blank.
    #[test]
    fn the_system_prompt_can_be_overridden_by_a_file() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        // No file yet: the built-in.
        assert_eq!(system_prompt(), DEFAULT_SYSTEM_PROMPT);

        let dir = tmp.path().join("amux");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("auto-prompt.md"), "  custom rules  \n").unwrap();
        assert_eq!(system_prompt(), "custom rules");

        // A blank file falls back rather than sending an empty system message.
        std::fs::write(dir.join("auto-prompt.md"), "   \n").unwrap();
        assert_eq!(system_prompt(), DEFAULT_SYSTEM_PROMPT);

        std::env::remove_var("XDG_CONFIG_HOME");
    }


    /// The whole path, against the real local model: flags, spawn, timeout,
    /// envelope, parse.
    ///
    /// Ignored by default because it starts Claude Code and spends a little of
    /// whoever's quota is configured. Run it after touching [`ask`] — the unit
    /// tests below check the parsing, and parsing was never the part that broke
    /// when the caller changed.
    ///
    ///     cargo test auto_asks_the_local_model -- --ignored --nocapture
    #[test]
    #[ignore]
    fn auto_asks_the_local_model_and_gets_an_instruction() {
        // Point config elsewhere so this exercises the prompt in this file
        // rather than whatever override the machine happens to carry — the
        // override is a copy that drifts, and a test of the default that reads
        // someone's copy tests nothing.
        let scratch = std::env::temp_dir().join("amux-auto-probe");
        let _ = std::fs::create_dir_all(&scratch);
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &scratch) };

        let tail = "running 3 tests ... 2 passed, 1 FAILED\n                    assert_eq!(left, right) at src/lib.rs:42";
        let decision = decide("keep going until the tests pass", tail, 1, 30, false);
        match &decision {
            // The work is plainly unfinished, so a model that read the tail at
            // all should say to carry on and say something about the failure.
            Decision::Continue(message) => {
                assert!(!message.trim().is_empty());
                assert!(
                    message.chars().count() <= MAX_MESSAGE_CHARS,
                    "the contract caps the message and this one is {} chars",
                    message.chars().count()
                );
                eprintln!("continue: {message}");
            }
            other => panic!("expected a continuation, got {other:?}"),
        }
    }
}
