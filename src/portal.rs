//! RemoteDesktop portal connection module
//!
//! This module handles the connection to the RemoteDesktop portal using ashpd,
//! requesting mouse and keyboard permissions, and monitoring portal signals.

use anyhow::{Context, Result};
use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
use log::{debug, warn};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

/// Portal session wrapper
pub struct PortalSession {
    fd: RawFd,
    pub(crate) clipboard: Clipboard<'static>,
    pub(crate) session_proxy: Session<'static, RemoteDesktop<'static>>,
}

impl PortalSession {
    /// Create a new portal session
    fn new(
        fd: RawFd,
        clipboard: Clipboard<'static>,
        session_proxy: Session<'static, RemoteDesktop<'static>>,
    ) -> Self {
        Self {
            fd,
            clipboard,
            session_proxy,
        }
    }

    /// Get the file descriptor for the EI connection
    pub fn ei_fd(&self) -> RawFd {
        self.fd
    }
}

/// Get the path to the restore token file
fn get_restore_token_path() -> PathBuf {
    let cache_dir = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".cache")
        });
    cache_dir.join("schengen").join("client-restore-token.txt")
}

/// Read the restore token from the cache file
fn read_restore_token() -> Option<String> {
    let token_path = get_restore_token_path();
    match std::fs::read_to_string(&token_path) {
        Ok(token) => {
            let token = token.trim().to_string();
            if token.is_empty() {
                debug!("Restore token file is empty");
                None
            } else {
                debug!("Read restore token from {:?}", token_path);
                Some(token)
            }
        }
        Err(e) => {
            debug!("Could not read restore token from {:?}: {}", token_path, e);
            None
        }
    }
}

/// Write the restore token to the cache file
fn write_restore_token(token: &str) {
    let token_path = get_restore_token_path();

    // Create parent directory if it doesn't exist
    if let Some(parent) = token_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        warn!("Failed to create cache directory {:?}: {}", parent, e);
        return;
    }

    match std::fs::write(&token_path, token) {
        Ok(_) => {
            debug!("Saved restore token to {:?}", token_path);
        }
        Err(e) => {
            warn!("Failed to write restore token to {:?}: {}", token_path, e);
        }
    }
}

/// Connect to the RemoteDesktop portal with mouse and keyboard permissions
///
/// This function uses ashpd to establish a connection to the RemoteDesktop portal,
/// requesting permission to send mouse and keyboard events. It creates a session
/// and obtains a file descriptor for the EI (emulated input) connection.
///
/// # Returns
///
/// Returns a `Session` object representing the portal session
///
/// # Errors
///
/// Returns an error if the portal connection fails or permissions are denied
pub async fn connect_remote_desktop() -> Result<PortalSession> {
    debug!("Connecting to RemoteDesktop portal");

    let proxy = RemoteDesktop::new()
        .await
        .context("Failed to create RemoteDesktop proxy")?;

    // Create a new session
    let session = proxy
        .create_session()
        .await
        .context("Failed to create RemoteDesktop session")?;

    debug!("RemoteDesktop session created");

    // Select devices (mouse and keyboard)
    let devices = DeviceType::Keyboard | DeviceType::Pointer;

    // Read existing restore token if available
    let restore_token = read_restore_token();

    proxy
        .select_devices(
            &session,
            devices,
            restore_token.as_deref(),       // restore_token
            PersistMode::ExplicitlyRevoked, // persist_mode
        )
        .await
        .context("Failed to select devices")?;

    debug!("Selected keyboard and pointer devices");

    // Create clipboard proxy and request access BEFORE starting the session
    let clipboard = Clipboard::new()
        .await
        .context("Failed to create Clipboard proxy")?;

    clipboard
        .request(&session)
        .await
        .context("Failed to request clipboard access")?;

    debug!("Clipboard access requested");

    // Start the session (use None for window identifier)
    let start_response = proxy
        .start(&session, None)
        .await
        .context("Failed to start RemoteDesktop session")?;

    // Save the restore token from the start response if provided
    if let Ok(selected_devices) = start_response.response()
        && let Some(token) = selected_devices.restore_token()
    {
        write_restore_token(token);
    }

    debug!("RemoteDesktop session started, connecting to EIS");

    // Connect to the EI (Emulated Input) socket
    let owned_fd = proxy
        .connect_to_eis(&session)
        .await
        .context("Failed to connect to EIS")?;

    let raw_fd = owned_fd.as_raw_fd();
    debug!(
        "RemoteDesktop portal connected successfully (fd: {})",
        raw_fd
    );

    // Keep the OwnedFd alive by leaking it (the FD will be used for the duration of the program)
    std::mem::forget(owned_fd);

    // Leak the proxy and session to get 'static lifetimes
    let _proxy_static: &'static RemoteDesktop = Box::leak(Box::new(proxy));
    let session_static = unsafe {
        // SAFETY: We're converting the session to 'static lifetime by leaking it
        // The session will live for the entire program duration
        std::mem::transmute::<
            Session<'_, RemoteDesktop<'_>>,
            Session<'static, RemoteDesktop<'static>>,
        >(session)
    };
    let clipboard_static = unsafe {
        // SAFETY: Same as above - clipboard will live for the program duration
        std::mem::transmute::<Clipboard<'_>, Clipboard<'static>>(clipboard)
    };

    Ok(PortalSession::new(raw_fd, clipboard_static, session_static))
}
