//! Toggle for the SFTP dual-pane overlay, plus the remote-target
//! resolution that decides which ssh session the overlay rides on.

use super::TermWindow;
use crate::overlay::sftp::{sftp_palette, SftpOverlayConfig};
use mux::pane::{CachePolicy, Pane};
use std::sync::Arc;

pub(super) fn toggle_overlay(term: &mut TermWindow, pane: &Arc<dyn Pane>) {
    // The active pane is the OVERLAY pane itself while the browser is
    // open; find the underlying pane that hosts it so the same
    // shortcut toggles the overlay closed.
    if let Some(host) = term.overlay_host_for_pane(pane) {
        term.cancel_overlay_for_pane(host);
        return;
    }

    let pane_id = pane.pane_id();
    // Clear any stale registry entry from a previous overlay run.
    crate::overlay::sftp::registry::unregister(pane_id);

    let Some(config) = resolve_overlay_config(term, pane) else {
        term.show_toast(
            "No SSH connection in this pane; open one with New Tab (SSH:host) or run ssh first"
                .to_string(),
        );
        return;
    };

    let (overlay, future) = crate::overlay::start_overlay_pane(term, pane, move |pane_id, term| {
        crate::overlay::sftp::sftp_overlay(pane_id, term, config)
    });
    term.assign_overlay_for_pane(pane_id, overlay);
    term.sftp_overlay_panes.insert(pane_id);

    promise::spawn::spawn(async move {
        if let Err(e) = future.await {
            log::error!("SFTP overlay error for pane {pane_id}: {e:#}");
        }
        crate::overlay::sftp::registry::unregister(pane_id);
    })
    .detach();
}

/// Resolve how the overlay reaches the remote side.  Resolution lives
/// in [crate::sftp_target] so drops and downloads see the same answer.
fn resolve_overlay_config(
    term: &mut TermWindow,
    pane: &Arc<dyn Pane>,
) -> Option<SftpOverlayConfig> {
    let target = crate::sftp_target::resolve(pane)?;
    Some(SftpOverlayConfig {
        session: target.session,
        connect_target: target.connect_target,
        reconnect_target: target.reconnect_target,
        remote_label: target.label,
        local_path: local_panel_path(pane),
        // Always open the remote side at its home directory.  Starting
        // from the shell's cwd was tried and reads as "my files are
        // gone": the cwd is often somewhere with nothing visible in it.
        // The shell's cwd is still where Finder drops land.
        remote_start: None,
        palette: sftp_palette(term.palette()),
    })
}

/// Local panel start directory: the pane's cwd when it is on this
/// machine, otherwise the user's home.
fn local_panel_path(pane: &Arc<dyn Pane>) -> String {
    let local_hostname = crate::local_hostname::current();
    if let Some(url) = pane.get_current_working_dir(CachePolicy::AllowStale) {
        if url.scheme() == "file"
            && crate::local_hostname::is_local_file_host(url.host_str(), local_hostname.as_deref())
        {
            return url.path().to_string();
        }
    }
    dirs_next::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|| "/".to_string())
}
