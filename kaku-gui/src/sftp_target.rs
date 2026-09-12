//! One place that answers "which ssh session, and which remote
//! directory, does this pane talk to?".
//!
//! The file browser, Finder drops, and one-key downloads all need the
//! same answer.  Keeping it here is what lets those features work
//! without the browser being open, and it is the single place to extend
//! when a new way to reach a host appears.

use mux::pane::{CachePolicy, Pane};
use std::sync::Arc;
use wezterm_ssh::Session;

pub(crate) struct RemoteTarget {
    /// Live, authenticated session, when one exists.
    pub session: Option<Session>,
    /// `user@host` (or the domain label) for messages.
    pub label: String,
    /// Directory to land in: the remote shell's reported cwd when there
    /// is one, otherwise the remote home (resolved by the caller).
    pub cwd: Option<String>,
    /// Target to dial when there is no live session yet.
    pub connect_target: Option<String>,
    /// Target to redial when a pooled session dies mid-session.
    pub reconnect_target: Option<String>,
}

/// Resolve the remote side of `pane`; None when the pane is local.
///
/// 1. A pane spawned from an `SSH:` domain reuses that domain's live,
///    already authenticated session.
/// 2. A pane running the ssh CLI reuses an already authenticated
///    session: this app run's pool, then any live SSH domain for the
///    same host.
/// 3. Otherwise the caller gets a target to connect to.
pub(crate) fn resolve(pane: &Arc<dyn Pane>) -> Option<RemoteTarget> {
    if let Some((session, label)) = mux::ssh::ssh_session_for_pane(pane) {
        return Some(RemoteTarget {
            session: Some(session),
            label,
            cwd: remote_cwd(pane),
            connect_target: None,
            reconnect_target: None,
        });
    }

    let target = crate::tabbar::ssh_login_target_for_real_pane(pane)?;
    if let Some(session) = crate::sftp_sessions::get(&target) {
        return Some(RemoteTarget {
            session: Some(session),
            label: target.clone(),
            cwd: remote_cwd(pane),
            // Keep the target so a dead pooled session can be redialed.
            connect_target: None,
            reconnect_target: Some(target),
        });
    }
    if let Some((session, label)) = mux::ssh::live_session_for_target(&target) {
        return Some(RemoteTarget {
            session: Some(session),
            label,
            cwd: remote_cwd(pane),
            connect_target: None,
            reconnect_target: None,
        });
    }
    Some(RemoteTarget {
        session: None,
        label: target.clone(),
        cwd: remote_cwd(pane),
        connect_target: Some(target.clone()),
        reconnect_target: Some(target),
    })
}

/// Remote working directory reported by the shell in `pane` (OSC 7),
/// when it is a path on another machine.  A local path means either the
/// pane is local or the remote shell never reported one, so there is
/// nothing to trust.
pub(crate) fn remote_cwd(pane: &Arc<dyn Pane>) -> Option<String> {
    let url = pane.get_current_working_dir(CachePolicy::AllowStale)?;
    if url.scheme() != "file" {
        return None;
    }
    let local_hostname = crate::local_hostname::current();
    if crate::local_hostname::is_local_file_host(url.host_str(), local_hostname.as_deref()) {
        return None;
    }
    Some(url.path().to_string())
}
