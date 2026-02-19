//! RemoteDesktop portal connection module
//!
//! This module handles the connection to the RemoteDesktop portal using ashpd,
//! requesting mouse and keyboard permissions, and monitoring portal signals.

use anyhow::{Context, Result};
use ashpd::desktop::PersistMode;
use ashpd::desktop::clipboard::{Clipboard, RequestClipboardOptions};
use ashpd::desktop::remote_desktop::{
    ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::{CreateSessionOptions, Session};
use log::{debug, warn};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

/// Portal session wrapper
pub struct PortalSession {
    fd: RawFd,
    pub(crate) clipboard: Option<Clipboard>,
    pub(crate) session_proxy: Session<RemoteDesktop>,
}

impl PortalSession {
    /// Create a new portal session
    fn new(fd: RawFd, clipboard: Option<Clipboard>, session_proxy: Session<RemoteDesktop>) -> Self {
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
    if let Some(parent) = token_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("Failed to create cache directory {:?}: {}", parent, e);
            return;
        }
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
        .create_session(CreateSessionOptions::default())
        .await
        .context("Failed to create RemoteDesktop session")?;

    debug!("RemoteDesktop session created");

    // Select devices (mouse and keyboard)
    let devices = DeviceType::Keyboard | DeviceType::Pointer;

    // Read existing restore token if available
    let restore_token = read_restore_token();

    let options = SelectDevicesOptions::default()
        .set_persist_mode(Some(PersistMode::ExplicitlyRevoked))
        .set_restore_token(restore_token.as_deref())
        .set_devices(devices);

    proxy
        .select_devices(&session, options)
        .await
        .context("Failed to select devices")?;

    debug!("Selected keyboard and pointer devices");

    // Create clipboard proxy and request access BEFORE starting the session
    // Only available for RemoteDesktop version 2 or higher
    // Try to initialize clipboard, but don't fail if it's not available
    let clipboard = {
        let clipboard_result = async {
            let clipboard = Clipboard::new().await?;
            clipboard
                .request(&session, RequestClipboardOptions::default())
                .await?;
            Ok::<_, anyhow::Error>(clipboard)
        }
        .await;

        match clipboard_result {
            Ok(clipboard) => {
                debug!("Clipboard access requested (RemoteDesktop version 2+)");
                Some(clipboard)
            }
            Err(e) => {
                debug!(
                    "Clipboard not available (RemoteDesktop version < 2 or error): {}",
                    e
                );
                None
            }
        }
    };

    // Start the session (use None for window identifier)
    let start_response = proxy
        .start(&session, None, StartOptions::default())
        .await
        .context("Failed to start RemoteDesktop session")?;

    // Save the restore token from the start response if provided
    if let Ok(selected_devices) = start_response.response() {
        if let Some(token) = selected_devices.restore_token() {
            write_restore_token(token);
        }
    }

    debug!("RemoteDesktop session started, connecting to EIS");

    // Connect to the EI (Emulated Input) socket
    let owned_fd = proxy
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .context("Failed to connect to EIS")?;

    let raw_fd = owned_fd.as_raw_fd();
    debug!(
        "RemoteDesktop portal connected successfully (fd: {})",
        raw_fd
    );

    // Keep the OwnedFd alive by leaking it (the FD will be used for the duration of the program)
    std::mem::forget(owned_fd);

    Ok(PortalSession::new(raw_fd, clipboard, session))
}
