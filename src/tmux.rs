/// POSIX-quote one argument for safe insertion into a shell command line.
fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Join an argv into a single shell command string with each arg quoted.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_args_unquoted() {
        assert_eq!(shell_join(&["claude".into(), "--yolo".into()]), "claude --yolo");
    }

    #[test]
    fn spaces_and_quotes_are_escaped() {
        let joined = shell_join(&["echo".into(), "a b".into()]);
        assert_eq!(joined, "echo 'a b'");
        let joined = shell_join(&["echo".into(), "it's".into()]);
        assert_eq!(joined, r#"echo 'it'\''s'"#);
    }
}
