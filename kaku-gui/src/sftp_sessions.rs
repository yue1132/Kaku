//! Process-wide pool of live ssh sessions keyed by `[user@]host`.
//!
//! Lets the SFTP overlay (and future remote features) reuse an already
//! authenticated connection instead of re-running the ssh handshake:
//! whatever the user types to reach a host costs at most one
//! authentication per app run.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use wezterm_ssh::Session;

fn pool() -> &'static Mutex<HashMap<String, Session>> {
    static POOL: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Normalize `[user@]host[:port]` into a stable pool key.
pub fn target_key(target: &str) -> String {
    target.trim().to_lowercase()
}

/// Passwords typed during this app run, keyed like the session pool.
///
/// A second connection to a password-only host would otherwise ask again
/// (the pool only helps once a session exists), and every parallel lane
/// needs its own connection.  Keeping the password in memory until the
/// app exits is what desktop sftp clients do; it is never written to
/// disk or logged, and a failed attempt forgets it so the user is asked
/// again instead of looping on a stale value.
fn passwords() -> &'static Mutex<HashMap<String, String>> {
    static PASSWORDS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    PASSWORDS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Take the remembered password for `target`, if any.  Taking it means a
/// rejected attempt cannot be retried from the same stale value.
pub fn take_password(target: &str) -> Option<String> {
    passwords().lock().unwrap().remove(&target_key(target))
}

/// Remember a password that just worked.
pub fn remember_password(target: &str, password: &str) {
    if !password.is_empty() {
        passwords()
            .lock()
            .unwrap()
            .insert(target_key(target), password.to_string());
    }
}

/// Forget the remembered password for `target`.
pub fn forget_password(target: &str) {
    passwords().lock().unwrap().remove(&target_key(target));
}

/// Returns a handle to the pooled session for `target`, if one is live.
pub fn get(target: &str) -> Option<Session> {
    pool().lock().unwrap().get(&target_key(target)).cloned()
}

/// Store a freshly authenticated session under `target`.
pub fn insert(target: &str, session: Session) {
    pool().lock().unwrap().insert(target_key(target), session);
}

/// Drop the pooled entry for `target` after its session died.
pub fn evict(target: &str) {
    pool().lock().unwrap().remove(&target_key(target));
}

/// Timeout for helper connection attempts.
const HELPER_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Build the ssh config map for `[user@]host`, resolving `Host` blocks
/// by their `Hostname` when the typed name matches no pattern (see the
/// SFTP overlay notes).
pub fn build_config_for_target(user: Option<&str>, host: &str) -> wezterm_ssh::ConfigMap {
    let mut cfg = wezterm_ssh::Config::new();
    cfg.add_default_config_files();
    let mut matched: Option<wezterm_ssh::ConfigMap> = None;
    for alias in cfg.enumerate_hosts() {
        let candidate = cfg.for_host(&alias);
        let resolved_host = candidate
            .get("hostname")
            .map(|s| s.as_str())
            .unwrap_or(&alias);
        if !resolved_host.eq_ignore_ascii_case(host) {
            continue;
        }
        let user_ok = match user {
            Some(u) => candidate
                .get("user")
                .map(|cu| cu.as_str() == u)
                .unwrap_or(true),
            None => true,
        };
        if user_ok {
            matched = Some(candidate);
            break;
        }
        if matched.is_none() {
            matched = Some(candidate);
        }
    }
    let mut cfg_map = matched.unwrap_or_else(|| cfg.for_host(host));
    if let Some(user) = user {
        cfg_map.insert("user".to_string(), user.to_string());
    }
    cfg_map.insert(
        "wezterm_ssh_backend".to_string(),
        match config::configuration().ssh_backend {
            config::SshBackend::Ssh2 => "ssh2",
            config::SshBackend::LibSsh => "libssh",
        }
        .to_string(),
    );
    cfg_map.insert("serveraliveinterval".to_string(), "15".to_string());
    cfg_map
}

/// Connect to `target` without any interactive prompt: agent/key auth
/// only.  Used for auxiliary transfer connections; password-only
/// servers fail here and callers fall back to the main session.
pub fn connect_noninteractive(target: &str) -> Result<Session, String> {
    use wezterm_ssh::SessionEvent;

    let (user, host) = match target.split_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, target.to_string()),
    };
    let cfg_map = build_config_for_target(user.as_deref(), &host);
    let (session, events) = Session::connect(cfg_map).map_err(|e| e.to_string())?;

    let mut working_password: Option<String> = None;
    let result: Result<Session, String> = smol::block_on(async {
        let deadline = Instant::now() + HELPER_CONNECT_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timed out".to_string());
            }
            match smol::future::or(
                async { events.recv().await.map_err(|e| e.to_string()) },
                async {
                    smol::Timer::after(remaining).await;
                    Err("timed out".to_string())
                },
            )
            .await
            {
                Ok(SessionEvent::Authenticated) => return Ok(session.clone()),
                Ok(SessionEvent::Banner(_)) => {}
                Ok(SessionEvent::HostVerify(ev)) => {
                    // The main session already trusts this host.
                    ev.try_answer(true).map_err(|e| e.to_string())?;
                }
                Ok(SessionEvent::Authenticate(ev)) => {
                    // A password the user typed for this host earlier in
                    // the app run lets auxiliary connections (parallel
                    // transfer lanes) authenticate without prompting.
                    let Some(password) = take_password(target) else {
                        return Err("password authentication required".to_string());
                    };
                    let answers = vec![password.clone(); ev.prompts.len().max(1)];
                    ev.answer(answers).await.map_err(|e| e.to_string())?;
                    working_password = Some(password);
                }
                Ok(SessionEvent::HostVerificationFailed(f)) => {
                    return Err(format!("host key verification failed: {}", f.key));
                }
                Ok(SessionEvent::Error(err)) => return Err(err),
                Err(err) => return Err(err.to_string()),
            }
        }
    });
    match (&result, &working_password) {
        (Ok(_), Some(password)) => remember_password(target, password),
        (Err(_), _) => forget_password(target),
        _ => {}
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_key_is_normalized() {
        assert_eq!(target_key("Root@10.10.1.55"), "root@10.10.1.55");
        assert_eq!(target_key(" root@Host "), "root@host");
    }

    #[test]
    fn insert_get_evict_roundtrip() {
        // The pool stores only what callers give it; a missing entry
        // stays missing without a real session to insert.
        evict("nobody@nowhere");
        assert!(get("nobody@nowhere").is_none());
    }

    /// A dotted host with no ssh config entry resolves to the local
    /// username; a target that keeps its `user@` must override it.  Without
    /// that, an ssh-CLI pane's transfer lands as the wrong account and every
    /// write comes back as SSH_FX_PERMISSION_DENIED (SFTP error code 3).
    #[test]
    fn typed_login_overrides_the_local_username() {
        let cfg = build_config_for_target(Some("admin"), "kaku-sftp-target.invalid");
        assert_eq!(cfg.get("user").map(|s| s.as_str()), Some("admin"));
    }
}
