//! Provider account quota: how much is left, or how much has been used.
//!
//! Two sources, both read-only:
//!
//! - **OpenCode Go**: `GET https://opencode.ai/zen/go/v1/usage` with the
//!   `opencode-go` key opencode already stores in its own `auth.json`. Answers
//!   with rolling / weekly / monthly percentages and their reset times, so
//!   "remaining" is `100 - percent`.
//! - **Sub2API**: log in with the account's email and password
//!   (`POST /api/v1/auth/login`), then read the account-wide dashboard stats
//!   (`GET /api/v1/usage/dashboard/stats`) for today's total cost. The
//!   dashboard binds a session to the client that opened it — refreshing from
//!   anywhere else answers `SESSION_BINDING_MISMATCH` — so amux logs in itself
//!   and keeps the token pair it was issued, rather than borrowing the
//!   browser's.
//!
//! Sub2API credentials live in `$XDG_CONFIG_HOME/amux/quota.toml`; the tokens
//! amux is issued are cached in `~/.amux/quota-tokens.json` (0600).

use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A normal browser user agent. Cloudflare in front of opencode.ai answers a
/// bot-like client with `403 error code: 1010`, which reads like a credential
/// failure rather than a blocked request.
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
                  AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

const OPENCODE_USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";

/// The numbers move slowly and each read is a network round trip.
const CACHE_TTL: Duration = Duration::from_secs(300);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

static CACHE: LazyLock<Mutex<Option<(Instant, serde_json::Value)>>> =
    LazyLock::new(|| Mutex::new(None));

/// Both providers' quota, or as much of it as could be read.
///
/// Cached for [`CACHE_TTL`]. A provider that cannot be read contributes `null`
/// rather than failing the call — one missing key should not hide the other.
pub fn snapshot() -> serde_json::Value {
    {
        let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return value.clone();
            }
        }
    }
    let opencode = opencode_go();
    let sub2api = sub2api();
    let value = serde_json::json!({
        "ok": opencode.is_some() || sub2api.is_some(),
        "opencodeGo": opencode,
        "sub2api": sub2api,
    });
    *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), value.clone()));
    value
}

fn client() -> Option<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(UA)
        .build()
        .ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn trimmed_base(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

// ---------------------------------------------------------------- opencode

/// The `opencode-go` API key opencode already stores for itself.
fn opencode_key() -> Option<String> {
    let home = dirs::home_dir()?;
    let text = std::fs::read_to_string(home.join(".local/share/opencode/auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("opencode-go")?
        .get("key")?
        .as_str()
        .map(str::to_string)
}

/// OpenCode Go's rolling / weekly / monthly usage windows.
fn opencode_go() -> Option<serde_json::Value> {
    let key = opencode_key()?;
    let resp = client()?
        .get(OPENCODE_USAGE_URL)
        .bearer_auth(key)
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    let usage = v.get("usage")?;
    let window = |name: &str| -> Option<serde_json::Value> {
        let w = usage.get(name)?;
        let percent = w.get("percent").and_then(|p| p.as_f64()).unwrap_or(0.0);
        Some(serde_json::json!({
            "percent": percent,
            "remainingPercent": (100.0 - percent).max(0.0),
            "status": w.get("status").cloned().unwrap_or(serde_json::Value::Null),
            "resetsAt": w.get("resetsAt").cloned().unwrap_or(serde_json::Value::Null),
        }))
    };
    Some(serde_json::json!({
        "rolling": window("rolling"),
        "weekly": window("weekly"),
        "monthly": window("monthly"),
    }))
}

// ---------------------------------------------------------------- sub2api

#[derive(Debug, Clone, serde::Deserialize)]
struct Sub2ApiConfig {
    base: String,
    email: String,
    password: String,
    /// IANA zone the dashboard buckets days by. Defaults to Asia/Shanghai,
    /// which is what the deployments amux talks to report in.
    #[serde(default)]
    timezone: Option<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_at_ms: u64,
}

impl Tokens {
    fn usable(&self) -> bool {
        !self.access_token.is_empty() && now_ms() + 60_000 < self.expires_at_ms
    }
}

/// `[sub2api]` from `$XDG_CONFIG_HOME/amux/quota.toml`, with the password
/// overridable by `AMUX_SUB2API_PASSWORD` so a secret need not sit in a file.
fn sub2api_config() -> Option<Sub2ApiConfig> {
    let path = crate::config::config_dir()?.join("quota.toml");
    let text = std::fs::read_to_string(path).ok()?;
    #[derive(serde::Deserialize)]
    struct Root {
        sub2api: Sub2ApiConfig,
    }
    let mut cfg = toml::from_str::<Root>(&text).ok()?.sub2api;
    if let Ok(password) = std::env::var("AMUX_SUB2API_PASSWORD") {
        if !password.is_empty() {
            cfg.password = password;
        }
    }
    if cfg.base.is_empty() || cfg.email.is_empty() || cfg.password.is_empty() {
        return None;
    }
    Some(cfg)
}

fn tokens_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".amux").join("quota-tokens.json"))
}

fn load_tokens() -> Tokens {
    tokens_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_tokens(tokens: &Tokens) {
    let Some(path) = tokens_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(tokens) {
        if std::fs::write(&path, text).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
}

fn tokens_from_response(resp: reqwest::blocking::Response) -> Option<Tokens> {
    let v: serde_json::Value = resp.json().ok()?;
    let data = v.get("data")?;
    let access_token = data.get("access_token")?.as_str()?.to_string();
    let refresh_token = data
        .get("refresh_token")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    let expires_in = data
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(86_400);
    Some(Tokens {
        access_token,
        refresh_token,
        expires_at_ms: now_ms() + expires_in * 1000,
    })
}

fn login(c: &reqwest::blocking::Client, cfg: &Sub2ApiConfig) -> Option<Tokens> {
    let resp = c
        .post(format!("{}/api/v1/auth/login", trimmed_base(&cfg.base)))
        .json(&serde_json::json!({ "email": cfg.email, "password": cfg.password }))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    tokens_from_response(resp)
}

fn refresh(c: &reqwest::blocking::Client, base: &str, refresh_token: &str) -> Option<Tokens> {
    let resp = c
        .post(format!("{}/api/v1/auth/refresh", trimmed_base(base)))
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    tokens_from_response(resp)
}

fn stats_request(
    c: &reqwest::blocking::Client,
    base: &str,
    timezone: &str,
    token: &str,
) -> Option<reqwest::blocking::Response> {
    c.get(format!(
        "{}/api/v1/usage/dashboard/stats",
        trimmed_base(base)
    ))
    .query(&[("timezone", timezone)])
    .bearer_auth(token)
    .send()
    .ok()
}

/// The account-wide totals Sub2API's dashboard shows, "today" included.
fn sub2api() -> Option<serde_json::Value> {
    let cfg = sub2api_config()?;
    let c = client()?;
    let timezone = cfg
        .timezone
        .clone()
        .unwrap_or_else(|| "Asia/Shanghai".to_string());

    let mut tokens = load_tokens();
    if !tokens.usable() {
        tokens = if tokens.refresh_token.is_empty() {
            login(&c, &cfg)
        } else {
            refresh(&c, &cfg.base, &tokens.refresh_token).or_else(|| login(&c, &cfg))
        }?;
        save_tokens(&tokens);
    }

    let mut resp = stats_request(&c, &cfg.base, &timezone, &tokens.access_token)?;
    if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
        // The token amux kept was rejected — it expired early, or the session
        // was bound elsewhere. Log in once more and retry before giving up.
        tokens = login(&c, &cfg)?;
        save_tokens(&tokens);
        resp = stats_request(&c, &cfg.base, &timezone, &tokens.access_token)?;
    }
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().ok()?;
    let data = v.get("data")?;
    Some(serde_json::json!({
        "todayCost": data.get("today_actual_cost"),
        "todayRequests": data.get("today_requests"),
        "todayTokens": data.get("today_tokens"),
        "totalCost": data.get("total_actual_cost"),
        "byPlatform": data.get("by_platform").cloned().unwrap_or(serde_json::Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_stays_usable_until_it_nearly_expires() {
        let mut tokens = Tokens {
            access_token: "jwt".into(),
            refresh_token: "rt".into(),
            expires_at_ms: now_ms() + 3_600_000,
        };
        assert!(tokens.usable());
        tokens.expires_at_ms = now_ms() + 30_000;
        assert!(
            !tokens.usable(),
            "a token about to expire must be refreshed"
        );
        tokens.access_token.clear();
        assert!(!tokens.usable());
    }

    #[test]
    fn base_urls_lose_their_trailing_slash() {
        assert_eq!(
            trimmed_base("https://sub.iotex.me/"),
            "https://sub.iotex.me"
        );
        assert_eq!(trimmed_base("https://sub.iotex.me"), "https://sub.iotex.me");
    }

    #[test]
    fn tokens_round_trip_through_json() {
        let tokens = Tokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at_ms: 42,
        };
        let text = serde_json::to_string(&tokens).unwrap();
        let back: Tokens = serde_json::from_str(&text).unwrap();
        assert_eq!(back.access_token, "a");
        assert_eq!(back.expires_at_ms, 42);
    }

    /// The credentials come from `quota.toml`, and an empty one reads as
    /// "not configured" rather than as a login with a blank password.
    #[test]
    fn quota_toml_is_read_and_an_empty_one_is_refused() {
        let _guard = crate::test_home::lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        let dir = tmp.path().join("amux");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("quota.toml"),
            "[sub2api]\nbase = \"https://sub.iotex.me\"\nemail = \"a@b.c\"\npassword = \"pw\"\n",
        )
        .unwrap();
        let cfg = sub2api_config().expect("a complete config should parse");
        assert_eq!(cfg.base, "https://sub.iotex.me");
        assert_eq!(cfg.email, "a@b.c");

        std::fs::write(dir.join("quota.toml"), "[sub2api]\nbase = \"x\"\n").unwrap();
        assert!(
            sub2api_config().is_none(),
            "a blank password is not a login"
        );

        match prev {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
