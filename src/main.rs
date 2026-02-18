//! Schengen Client - A CLI client for connecting to Synergy-compatible servers via libei
//!
//! This application connects to a Synergy/Deskflow server and forwards input events through the
//! RemoteDesktop portal using the libei protocol.

use anyhow::{Context, Result};
use clap::Parser;
use futures_util::StreamExt;
use listenfd::ListenFd;
use log::{debug, info, warn};
use schengen::client::{Builder, ClientEvent};
use std::time::Duration;

mod ei;
mod keymap;
mod portal;

/// Retry configuration for server connections
#[derive(Debug, Clone)]
struct RetryConfig {
    delay_ms: u64,
    max_retries: Option<usize>,
}

impl RetryConfig {
    fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            // --retry with no value: default to 3000ms
            return Ok(RetryConfig {
                delay_ms: 3000,
                max_retries: None,
            });
        }

        // Check for colon-separated format: delay:max_retries
        if let Some(colon_pos) = s.find(':') {
            let delay_part = &s[..colon_pos];
            let max_retries_part = &s[colon_pos + 1..];

            let delay_ms = delay_part
                .parse::<u64>()
                .context(format!("Invalid retry delay: {}", delay_part))?;

            let max_retries = max_retries_part
                .parse::<usize>()
                .context(format!("Invalid max retries: {}", max_retries_part))?;

            Ok(RetryConfig {
                delay_ms,
                max_retries: Some(max_retries),
            })
        } else {
            // Just delay value
            let delay_ms = s
                .parse::<u64>()
                .context(format!("Invalid retry delay: {}", s))?;

            Ok(RetryConfig {
                delay_ms,
                max_retries: None,
            })
        }
    }
}

/// Schengen client for connecting to Synergy servers
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Synergy server address (host or host:port, defaults to port 24801).
    /// Optional when using systemd socket activation.
    #[arg(value_name = "HOST[:PORT]")]
    server: Option<String>,

    /// Client name (defaults to hostname). This is the name
    /// advertised to the Synergy server - the server must be configured
    /// to accept a client with that name.
    #[arg(short, long)]
    name: Option<String>,

    /// Retry connecting to server if initial connection fails.
    /// With no value: retry after 3000ms indefinitely.
    /// With value: --retry=300 retries after 300ms indefinitely.
    /// With colon: --retry=300:5 retries after 300ms for max 5 retries.
    #[arg(long, value_name = "DELAY_MS[:MAX_RETRIES]", default_missing_value = "", num_args = 0..=1)]
    retry: Option<String>,

    /// Increase verbosity (-v for info, -vv for debug, -vvv for trace)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

impl Args {
    fn get_client_name(&self) -> Result<String> {
        if let Some(ref name) = self.name {
            Ok(name.clone())
        } else {
            hostname::get()
                .context("Failed to get hostname")?
                .into_string()
                .map_err(|_| anyhow::anyhow!("Hostname contains invalid UTF-8"))
        }
    }

    fn get_log_level(&self) -> log::LevelFilter {
        match self.verbose {
            0 => log::LevelFilter::Warn,
            1 => log::LevelFilter::Info,
            2 => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        }
    }

    fn get_retry_config(&self) -> Result<Option<RetryConfig>> {
        match &self.retry {
            None => Ok(None),
            Some(s) => Ok(Some(RetryConfig::parse(s)?)),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Detect if we're running under systemd socket activation
    // Check for LISTEN_FDS environment variable that systemd sets
    let is_socket_activated = std::env::var("LISTEN_FDS").is_ok();

    // Use plain logging for systemd (no colors, no timestamps - journald adds those)
    // Use colorful logging for interactive/direct use
    env_logger::Builder::new()
        .filter_level(args.get_log_level())
        .format(move |buf, record| {
            use std::io::Write;

            if is_socket_activated {
                // Plain format for systemd
                writeln!(buf, "{:5} - {}", record.level(), record.args())
            } else {
                // Colorful format with timestamp for interactive use
                const BLUE: &str = "\x1b[34m";
                const GREEN: &str = "\x1b[32m";
                const MAGENTA: &str = "\x1b[35m";
                const RESET: &str = "\x1b[0m";

                let color = if record.target().contains("synergy") {
                    BLUE
                } else if record.target().contains("ei") {
                    GREEN
                } else if record.target().contains("portal") {
                    MAGENTA
                } else {
                    ""
                };

                writeln!(
                    buf,
                    "{} - {:5} - {}{}{}",
                    chrono::Local::now().format("%H:%M:%S"),
                    record.level(),
                    color,
                    record.args(),
                    if color.is_empty() { "" } else { RESET }
                )
            }
        })
        .init();

    let client_name = args.get_client_name()?;

    info!("Starting schengen-client as '{}'", client_name);

    // We need to connect to the portal first and receive the devices. We're not
    // guaranteed to get any, it's a potential user interaction dialog, so let'
    // make sure we *can* actually emulate input before we connect to the server.
    info!("Step 1/4: Connecting to RemoteDesktop portal...");
    let portal_session = portal::connect_remote_desktop().await?;

    info!("Step 2/4: Connecting to libei...");
    let mut ei_context = ei::connect_with_fd(portal_session.ei_fd()).await?;

    // Step 3: Wait for devices to be received and resumed
    info!("Step 3/4: Waiting for EI devices...");
    let max_wait = std::time::Duration::from_secs(10);
    let start = std::time::Instant::now();
    let mut devices_ready = false;

    while start.elapsed() < max_wait {
        // Process EI events to get device configuration
        match ei_context.recv_event().await {
            Ok(_) => {
                let has_keyboard = ei_context.has_keyboard();
                let has_pointer = ei_context.has_pointer();

                if has_keyboard && has_pointer {
                    devices_ready = true;
                    break;
                }
                // No required devices yet, sleep a bit and try again
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
            Err(e) => {
                warn!("EI error while waiting for devices: {}", e);
                break;
            }
        }
    }

    if !devices_ready {
        warn!(
            "Required devices (keyboard + pointer) not ready after waiting - continuing anyway, input may not work"
        );
    }

    // Get screen dimensions from all EI device regions
    let (_x, _y, width, height) = ei_context.get_screen_dimensions();
    debug!(
        "Screen dimensions from EI devices: {}x{} at ({}, {})",
        width, height, _x, _y
    );

    // Step 4: Connect to or accept from Synergy server
    info!("Step 4/4: Establishing Synergy connection...");
    let retry_config = args.get_retry_config()?;
    let client = {
        // Check for systemd socket activation
        let mut listen_fd = ListenFd::from_env();

        if let Some(listener) = listen_fd.take_tcp_listener(0)? {
            info!("Using systemd socket activation (listening on inherited socket)");
            listener.set_nonblocking(true)?;
            let tokio_listener = tokio::net::TcpListener::from_std(listener)?;

            debug!("Waiting for incoming connection from Synergy server...");
            let (stream, peer_addr) = tokio_listener.accept().await?;
            debug!("✓ Accepted connection from {}", peer_addr);

            // Use connect_with_stream for socket activation
            Builder::new()
                .name(&client_name)
                .dimensions(width, height)
                .connect_with_stream(stream)
                .await
                .context("Failed to establish connection with existing stream")?
        } else if let Some(server) = &args.server {
            // Normal mode: connect to server with schengen client's built-in retry
            debug!("Connecting to Synergy server: {}", server);

            let mut builder = Builder::new()
                .server_addr(server)?
                .name(&client_name)
                .dimensions(width, height);

            // Map retry config to schengen API
            if let Some(config) = retry_config {
                builder = builder.retry_interval(Duration::from_millis(config.delay_ms));
                if let Some(max) = config.max_retries {
                    builder = builder.retry_count(max);
                }
            } else {
                // No retry - single attempt
                builder = builder.retry_count(1);
            }

            builder
                .connect()
                .await
                .context("Failed to connect to server")?
        } else {
            return Err(anyhow::anyhow!(
                "No server address specified and no socket activation detected. \
                 Either provide a server address or run under systemd socket activation."
            ));
        }
    };
    info!("All connections established successfully");

    run_event_loop(client, portal_session, ei_context).await?;

    info!("Shutting down");
    Ok(())
}

/// Handle a client event from the schengen client and forward to EI or handle clipboard
async fn handle_client_event(
    ei_context: &mut ei::Context,
    portal_session: &portal::PortalSession,
    last_sequence: &mut u32,
    clipboard_data: &mut Option<String>,
    event: ClientEvent,
) -> Result<()> {
    use schengen::protocol::*;

    match event {
        ClientEvent::CursorEntered {
            x,
            y,
            sequence_number,
            modifier_mask,
        } => {
            *last_sequence = sequence_number;
            let msg = Message::CursorEntered(MessageCursorEntered {
                x,
                y,
                sequence: sequence_number,
                mask: modifier_mask,
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::CursorLeft => {
            let msg = Message::CursorLeft(MessageCursorLeft);
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::MouseMove { x, y } => {
            let msg = Message::MouseMove(MessageMouseMove { x, y });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::MouseRelativeMove { dx, dy } => {
            let msg = Message::MouseRelativeMove(MessageMouseRelativeMove { x: dx, y: dy });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::MouseButtonDown { button } => {
            let msg = Message::MouseButtonDown(MessageMouseButtonDown { button });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::MouseButtonUp { button } => {
            let msg = Message::MouseButtonUp(MessageMouseButtonUp { button });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::MouseWheel { horiz, vert } => {
            let msg = Message::MouseWheel(MessageMouseWheel {
                xdelta: horiz,
                ydelta: vert,
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::KeyDown { key, mask, button } => {
            let msg = Message::KeyDown(MessageKeyDown {
                keyid: key,
                mask,
                button,
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::KeyUp { key, mask, button } => {
            let msg = Message::KeyUp(MessageKeyUp {
                keyid: key,
                mask,
                button,
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::KeyRepeat {
            key,
            mask,
            count,
            button,
        } => {
            let msg = Message::KeyRepeat(MessageKeyRepeat {
                keyid: key,
                mask,
                count,
                button,
                lang: LengthPrefixedString(String::new()), // Language info not available in ClientEvent
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::ClipboardData { data, .. } => {
            // Handle clipboard data from server
            let text =
                String::from_utf8(data).context("Received non-UTF8 clipboard data from server")?;

            debug!("Received clipboard data from server ({} bytes)", text.len());

            // Store the data
            *clipboard_data = Some(text);

            // Claim clipboard ownership
            let mime_types = &["text/plain;charset=utf-8", "text/plain"];
            debug!("Calling set_selection with mime_types: {:?}", mime_types);
            // FIXME: bug in ashpd 0.13.2 - missing mime types
            // https://github.com/bilelmoussaoui/ashpd/pull/362
            let opts = ashpd::desktop::clipboard::SetSelectionOptions::default();
            match portal_session
                .clipboard
                .set_selection(&portal_session.session_proxy, opts)
                .await
            {
                Ok(_) => {
                    debug!("✓ set_selection succeeded - clipboard ownership claimed");
                }
                Err(e) => {
                    warn!("Failed to call set_selection: {}", e);
                }
            }
        }

        ClientEvent::ScreenSaverChanged { active } => {
            let msg = Message::ScreenSaverChange(MessageScreenSaverChange {
                state: if active { 1 } else { 0 },
            });
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::ResetOptions => {
            let msg = Message::ResetOptions(MessageResetOptions);
            ei::handle_synergy_message(ei_context, msg).await?;
        }

        ClientEvent::SetOptions => {
            // SetOptions doesn't have a direct Message equivalent, skip for now
            debug!("Received SetOptions event (not implemented)");
        }

        ClientEvent::Close => {
            info!("Server closed connection");
        }
    }

    Ok(())
}

/// Main event loop that handles messages from all sources
async fn run_event_loop(
    mut client: schengen::client::Client,
    portal_session: portal::PortalSession,
    mut ei_context: ei::Context,
) -> Result<()> {
    use schengen::protocol::*;
    // Track the last sequence number from CursorEntered for clipboard messages
    let mut last_sequence = 0u32;

    // Store clipboard data received from Synergy (for serving when requested)
    let mut clipboard_data: Option<String> = None;

    // Create clipboard monitoring stream directly from the clipboard
    debug!("Setting up clipboard monitoring...");
    let mut clipboard_stream = Box::pin(
        portal_session
            .clipboard
            .receive_selection_owner_changed()
            .await
            .context("Failed to create clipboard stream")?,
    );
    debug!("✓ Clipboard monitoring ready");

    // Create clipboard transfer request stream (for when someone requests our clipboard)
    debug!("Setting up clipboard transfer monitoring...");
    let mut clipboard_transfer_stream = Box::pin(
        portal_session
            .clipboard
            .receive_selection_transfer()
            .await
            .context("Failed to create clipboard transfer stream")?,
    );
    debug!("✓ Clipboard transfer monitoring ready");

    loop {
        tokio::select! {
            // Handle Synergy server events
            result = client.recv_event() => {
                match result {
                    Ok(event) => {
                        debug!("← Synergy event: {:?}", event);

                        handle_client_event(
                            &mut ei_context,
                            &portal_session,
                            &mut last_sequence,
                            &mut clipboard_data,
                            event,
                        ).await?;
                    }
                    Err(e) => {
                        warn!("Synergy connection error: {}", e);
                        break;
                    }
                }
            }

            // Portal signal handling is disabled for now because it conflicts with
            // clipboard monitoring (both need to borrow portal_session)
            // TODO: Implement proper portal signal handling if needed

            // Handle libei events
            result = ei_context.recv_event() => {
                match result {
                    Ok(Some(_event)) => {
                        // EI events are processed in recv_event, just continue
                        // Important: device state changes (paused/resumed) happen here
                    }
                    Ok(None) => {
                        // No event available
                    }
                    Err(e) => {
                        warn!("EI error: {}", e);
                        break;
                    }
                }
            }

            // Handle clipboard transfer requests (someone is requesting our clipboard data)
            Some((session, mime_type, serial)) = clipboard_transfer_stream.next() => {
                debug!("━━━ Clipboard transfer requested ━━━");
                debug!("  mime_type: '{}'", mime_type);
                debug!("  serial: {}", serial);
                debug!("  session path: {:?}", session);

                if let Some(ref data) = clipboard_data {
                    debug!("  We have data: {} bytes", data.len());
                    debug!("Calling selection_write with transfer session and serial={}", serial);

                    match portal_session.clipboard.selection_write(&session, serial).await {
                        Ok(owned_fd) => {
                            debug!("✓ selection_write succeeded, got fd");
                            use std::io::Write;

                            // Convert zvariant::OwnedFd → std::os::fd::OwnedFd → File
                            let std_fd: std::os::fd::OwnedFd = owned_fd.into();
                            let mut file = std::fs::File::from(std_fd);
                            debug!("  Writing {} bytes to fd", data.len());

                            match file.write_all(data.as_bytes()) {
                                Ok(_) => {
                                    debug!("✓ Wrote data successfully");
                                    if let Err(e) = portal_session.clipboard.selection_write_done(&session, serial, true).await {
                                        warn!("Failed to call selection_write_done: {}", e);
                                    } else {
                                        debug!("✓ Clipboard transfer complete!");
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to write data to fd: {}", e);
                                    let _ = portal_session.clipboard.selection_write_done(&session, serial, false).await;
                                }
                            }
                        }
                        Err(e) => {
                            warn!("✗ selection_write failed: {}", e);
                        }
                    }
                } else {
                    warn!("Clipboard transfer requested but we have no data!");
                }
            }

            // Handle clipboard changes
            Some((_session, change_event)) = clipboard_stream.next() => {
                debug!("Clipboard selection owner changed");

                // Check if we own the clipboard - if so, ignore this event
                // (we don't want to read our own clipboard that we just set)
                if change_event.session_is_owner().unwrap_or(false) {
                    debug!("  We own the clipboard, ignoring change event");
                    continue;
                }

                // Get the available mime types
                let mime_types = change_event.mime_types();
                debug!("  Available mime types: {:?}", mime_types);

                // Find a mime type that Synergy supports (text/plain variants)
                let supported_types = ["text/plain;charset=utf-8", "text/plain"];
                let selected_mime = mime_types.iter()
                    .find(|mime| supported_types.contains(&mime.as_str()))
                    .map(|s| s.as_str());

                if let Some(mime_type) = selected_mime {
                    debug!("  Reading clipboard with mime type: {}", mime_type);

                    // Read clipboard data
                    use std::io::Read;

                    match portal_session.clipboard.selection_read(&portal_session.session_proxy, mime_type).await {
                        Ok(owned_fd) => {
                            // Convert zvariant::OwnedFd → std::os::fd::OwnedFd → File
                            let std_fd: std::os::fd::OwnedFd = owned_fd.into();
                            let mut file = std::fs::File::from(std_fd);
                            let mut data = Vec::new();
                            match file.read_to_end(&mut data) {
                                Ok(_) => {
                                    match String::from_utf8(data) {
                                        Ok(text) => {
                                            debug!("Read {} bytes from clipboard", text.len());

                                            // Send clipboard claim message
                                            let claim_msg = Message::ClientClipboard(
                                                MessageClientClipboard {
                                                    id: 0, // 0 = primary clipboard
                                                    sequence: last_sequence,
                                                }
                                            );
                                            if let Err(e) = client.send(claim_msg).await {
                                                warn!("Failed to send clipboard claim: {}", e);
                                            } else {
                                                // Send clipboard data
                                                let data_msg = Message::ClipboardData(
                                                    MessageClipboardData {
                                                        id: 0,
                                                        sequence: last_sequence,
                                                        mark: 0, // Single chunk
                                                        data: LengthPrefixedString(text),
                                                    }
                                                );
                                                if let Err(e) = client.send(data_msg).await {
                                                    warn!("Failed to send clipboard data: {}", e);
                                                } else {
                                                    debug!("Clipboard data sent to Synergy server");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Clipboard data is not valid UTF-8: {}", e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to read clipboard data from fd: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            warn!("Failed to read clipboard: {}", e);
                        }
                    }
                } else {
                    debug!("  No supported text mime types available, skipping clipboard read");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_mapping() {
        let args = Args {
            server: Some("127.0.0.1:24800".to_string()),
            name: None,
            retry: None,
            verbose: 0,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Warn);

        let args = Args {
            server: Some("127.0.0.1:24800".to_string()),
            name: None,
            retry: None,
            verbose: 1,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Info);

        let args = Args {
            server: Some("127.0.0.1:24800".to_string()),
            name: None,
            retry: None,
            verbose: 2,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Debug);

        let args = Args {
            server: Some("127.0.0.1:24800".to_string()),
            name: None,
            retry: None,
            verbose: 3,
        };
        assert_eq!(args.get_log_level(), log::LevelFilter::Trace);
    }

    #[test]
    fn test_get_client_name_with_override() {
        let args = Args {
            server: Some("127.0.0.1:24800".to_string()),
            name: Some("test-client".to_string()),
            retry: None,
            verbose: 0,
        };
        assert_eq!(args.get_client_name().unwrap(), "test-client");
    }

    #[test]
    fn test_retry_config_default() {
        let config = RetryConfig::parse("").unwrap();
        assert_eq!(config.delay_ms, 3000);
        assert_eq!(config.max_retries, None);
    }

    #[test]
    fn test_retry_config_delay_only() {
        let config = RetryConfig::parse("300").unwrap();
        assert_eq!(config.delay_ms, 300);
        assert_eq!(config.max_retries, None);
    }

    #[test]
    fn test_retry_config_delay_with_max() {
        let config = RetryConfig::parse("300:5").unwrap();
        assert_eq!(config.delay_ms, 300);
        assert_eq!(config.max_retries, Some(5));
    }

    #[test]
    fn test_retry_config_invalid_delay() {
        assert!(RetryConfig::parse("abc").is_err());
    }

    #[test]
    fn test_retry_config_invalid_max() {
        assert!(RetryConfig::parse("300:abc").is_err());
    }
}
