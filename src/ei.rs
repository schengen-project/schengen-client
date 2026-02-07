//! Emulated Input (EI) protocol module
//!
//! This module handles the libei protocol connection for sending emulated
//! input events (keyboard and mouse) through the RemoteDesktop portal.
//! It also provides conversion functions from Synergy protocol messages to EI events.

use anyhow::{Context as AnyhowContext, Result};
use log::{debug, info, warn};
use reis::PendingRequestResult;
use reis::ei;
use reis::handshake::ei_handshake_blocking;
use schengen::protocol::Message;
use std::collections::HashMap;
use std::os::unix::io::FromRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::keymap::KeymapProcessor;

/// Translate Synergy keysyms to standard X11 keysyms
///
/// Synergy sends modifier keys with keysyms that are 0x1000 lower than standard X11.
/// This function corrects that offset for the modifier key range.
fn translate_synergy_keysym(synergy_keysym: u32) -> u32 {
    // Synergy modifier keysyms range: 0xefe0 - 0xefef
    // Standard X11 keysyms range:     0xffe0 - 0xffef
    // The difference is exactly 0x1000
    if (0xefe0..=0xefef).contains(&synergy_keysym) {
        synergy_keysym + 0x1000
    } else {
        synergy_keysym
    }
}

/// Region information for a device
#[derive(Debug, Clone)]
struct Region {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    scale: f32,
}

/// Information about a device being configured
#[derive(Debug, Clone)]
struct PendingDeviceInfo {
    device: ei::Device,
    name: Option<String>,
    pointer: Option<ei::Pointer>,
    pointer_absolute: Option<ei::PointerAbsolute>,
    keyboard: Option<ei::Keyboard>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    regions: Vec<Region>,
    keymap: Option<Arc<KeymapProcessor>>,
}

impl PendingDeviceInfo {
    fn new(device: ei::Device) -> Self {
        Self {
            device,
            name: None,
            pointer: None,
            pointer_absolute: None,
            keyboard: None,
            button: None,
            scroll: None,
            regions: Vec::new(),
            keymap: None,
        }
    }
}

/// Information about a configured device ready to send events
#[derive(Debug, Clone)]
struct DeviceInfo {
    device: ei::Device,
    name: String,
    pointer: Option<ei::Pointer>,
    pointer_absolute: Option<ei::PointerAbsolute>,
    keyboard: Option<ei::Keyboard>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    regions: Vec<Region>,
    paused: bool,
    keymap: Option<Arc<KeymapProcessor>>,
}

/// EI context wrapper for managing the libei connection
pub struct Context {
    context: ei::Context,
    connection: Option<ei::Connection>,
    seats: HashMap<String, ei::Seat>,
    capabilities: HashMap<String, u64>,
    pending_devices: HashMap<u64, PendingDeviceInfo>,
    devices: Vec<DeviceInfo>,
}

impl Context {
    /// Create a new EI context from a UnixStream
    fn new(context: ei::Context) -> Self {
        Self {
            context,
            connection: None,
            seats: HashMap::new(),
            capabilities: HashMap::new(),
            pending_devices: HashMap::new(),
            devices: Vec::new(),
        }
    }

    /// Perform the EI handshake
    async fn handshake(&mut self) -> Result<()> {
        debug!("Starting EI handshake");

        let handshake_resp = ei_handshake_blocking(
            &self.context,
            "schengen-client",
            ei::handshake::ContextType::Sender,
        )
        .context("EI handshake failed")?;

        debug!("EI handshake completed successfully");
        debug!(
            "Negotiated interfaces: {:?}",
            handshake_resp.negotiated_interfaces
        );

        self.connection = Some(handshake_resp.connection);
        Ok(())
    }

    /// Process pending events and bind to seats
    async fn process_events(&mut self) -> Result<()> {
        // Read from socket
        self.context.read()?;

        // Process all pending events
        while let Some(pending) = self.context.pending_event() {
            let event = match pending {
                PendingRequestResult::Request(event) => event,
                PendingRequestResult::ParseError(e) => {
                    return Err(anyhow::anyhow!("Failed to parse EI event: {:?}", e));
                }
                PendingRequestResult::InvalidObject(id) => {
                    return Err(anyhow::anyhow!("Invalid object ID: {}", id));
                }
            };

            self.handle_event(event)?;
        }

        Ok(())
    }

    /// Handle a single EI event
    fn handle_event(&mut self, event: ei::Event) -> Result<()> {
        match event {
            ei::Event::Connection(_connection, conn_event) => {
                self.handle_connection_event(conn_event)?;
            }
            ei::Event::Seat(seat, seat_event) => {
                self.handle_seat_event(seat, seat_event)?;
            }
            ei::Event::Device(device, device_event) => {
                self.handle_device_event(device, device_event)?;
            }
            ei::Event::Pingpong(pingpong, _event) => {
                debug!("Received pingpong, responding with done");
                pingpong.done(0);
                self.context.flush()?;
            }
            ei::Event::Handshake(_handshake, _event) => {
                warn!("Received unexpected handshake event after handshake completed");
            }
            ei::Event::Callback(_callback, _event) => {
                debug!("Received callback event");
            }
            ei::Event::Keyboard(keyboard, keyboard_event) => {
                self.handle_keyboard_event(keyboard, keyboard_event)?;
            }
            _ => {
                debug!("Received other EI event");
            }
        }

        Ok(())
    }

    /// Handle connection events
    fn handle_connection_event(&mut self, event: ei::connection::Event) -> Result<()> {
        match event {
            ei::connection::Event::Seat { seat: _ } => {
                debug!("✓ EI Connection: Received seat from connection");
                // Seat will be configured through subsequent events
            }
            ei::connection::Event::Ping { ping } => {
                debug!("Received ping, responding with done");
                ping.done(0);
                self.context.flush()?;
            }
            ei::connection::Event::Disconnected {
                last_serial,
                reason,
                explanation,
            } => {
                warn!(
                    "EI connection disconnected: last_serial={}, reason={:?}, explanation={:?}",
                    last_serial, reason, explanation
                );
            }
            ei::connection::Event::InvalidObject {
                last_serial,
                invalid_id,
            } => {
                warn!(
                    "Invalid object: last_serial={}, invalid_id={}",
                    last_serial, invalid_id
                );
            }
            _ => {
                debug!("Received other connection event");
            }
        }

        Ok(())
    }

    /// Handle seat events and bind when seat is ready
    fn handle_seat_event(&mut self, seat: ei::Seat, event: ei::seat::Event) -> Result<()> {
        match event {
            ei::seat::Event::Name { name } => {
                debug!("✓ EI Seat: '{}'", name);
            }
            ei::seat::Event::Capability { mask, interface } => {
                debug!("  - Capability: {} (mask=0x{:x})", interface, mask);
                self.capabilities.insert(interface, mask);
            }
            ei::seat::Event::Done => {
                debug!("✓ Seat configuration complete, binding to seat");
                self.bind_to_seat(seat)?;
            }
            ei::seat::Event::Device { device } => {
                let device_id = self.get_device_id(&device);
                debug!(
                    "  - Received new device from seat (device_id={})",
                    device_id
                );
                self.pending_devices
                    .insert(device_id, PendingDeviceInfo::new(device));
                debug!(
                    "  - Added to pending_devices (total pending: {})",
                    self.pending_devices.len()
                );
            }
            ei::seat::Event::Destroyed { serial } => {
                debug!("Seat destroyed (serial={})", serial);
            }
            _ => {
                debug!("Received other seat event");
            }
        }

        Ok(())
    }

    /// Bind to a seat with keyboard and pointer capabilities
    fn bind_to_seat(&mut self, seat: ei::Seat) -> Result<()> {
        debug!("Binding to seat with keyboard and pointer capabilities");

        // Calculate the capability mask for keyboard and pointer
        let mut capabilities_mask = 0u64;

        // Add keyboard capability if available
        if let Some(&mask) = self.capabilities.get("ei_keyboard") {
            capabilities_mask |= mask;
            debug!("Added keyboard capability: 0x{:x}", mask);
        }

        // Add pointer capability if available
        if let Some(&mask) = self.capabilities.get("ei_pointer") {
            capabilities_mask |= mask;
            debug!("Added pointer capability: 0x{:x}", mask);
        }

        // Add pointer_absolute capability if available
        if let Some(&mask) = self.capabilities.get("ei_pointer_absolute") {
            capabilities_mask |= mask;
            debug!("Added pointer_absolute capability: 0x{:x}", mask);
        }

        // Add button capability if available
        if let Some(&mask) = self.capabilities.get("ei_button") {
            capabilities_mask |= mask;
            debug!("Added button capability: 0x{:x}", mask);
        }

        // Add scroll capability if available
        if let Some(&mask) = self.capabilities.get("ei_scroll") {
            capabilities_mask |= mask;
            debug!("Added scroll capability: 0x{:x}", mask);
        }

        if capabilities_mask == 0 {
            warn!("No suitable capabilities available on seat");
            return Ok(());
        }

        debug!(
            "Binding to seat with capability mask: 0x{:x}",
            capabilities_mask
        );

        // Bind to the seat with the combined capabilities
        seat.bind(capabilities_mask);

        // Flush to send the bind request
        self.context.flush()?;

        // Store the seat for later use
        self.seats.insert("default".to_string(), seat);

        debug!("Successfully bound to seat");

        Ok(())
    }

    /// Find pending device - since devices are configured one at a time,
    /// we can just return the first (and typically only) pending device
    fn get_current_pending_device_mut(&mut self) -> Option<&mut PendingDeviceInfo> {
        // Devices are configured one at a time, so there should typically be only one
        self.pending_devices.values_mut().next()
    }

    /// Get a unique ID for a device (for logging purposes)
    fn get_device_id(&self, device: &ei::Device) -> u64 {
        // Just use pointer address for logging
        device as *const _ as usize as u64
    }

    /// Handle device events and store device interfaces
    fn handle_device_event(&mut self, device: ei::Device, event: ei::device::Event) -> Result<()> {
        let _device_id = self.get_device_id(&device);

        match event {
            ei::device::Event::Name { name } => {
                debug!("  Device name: '{}'", name);
                // Find the currently configuring pending device
                if let Some(pending) = self.get_current_pending_device_mut() {
                    pending.name = Some(name.clone());
                    debug!("  ✓ Name set for pending device");
                } else {
                    warn!("  Device name received but no pending device found!");
                }
            }
            ei::device::Event::DeviceType { device_type } => {
                debug!("Device type: {:?}", device_type);
            }
            ei::device::Event::Dimensions { width, height } => {
                debug!("Device dimensions: {}x{}", width, height);
                // Store as a single region covering the entire device
                if let Some(pending) = self.get_current_pending_device_mut() {
                    // If we get dimensions without explicit regions, create a default region
                    if pending.regions.is_empty() {
                        pending.regions.push(Region {
                            x: 0,
                            y: 0,
                            width,
                            height,
                            scale: 1.0,
                        });
                    }
                }
            }
            ei::device::Event::Region {
                offset_x,
                offset_y,
                width,
                hight, // Note: typo in the protocol binding
                scale,
            } => {
                debug!(
                    "Device region: offset=({}, {}), size={}x{}, scale={}",
                    offset_x, offset_y, width, hight, scale
                );
                if let Some(pending) = self.get_current_pending_device_mut() {
                    pending.regions.push(Region {
                        x: offset_x,
                        y: offset_y,
                        width,
                        height: hight,
                        scale,
                    });
                }
            }
            ei::device::Event::Interface { object } => {
                let interface_name = object.interface();
                debug!("Device interface: {}", interface_name);

                if let Some(pending) = self.get_current_pending_device_mut() {
                    // Try to downcast to each interface type
                    match interface_name {
                        "ei_pointer" => {
                            if let Some(pointer) = object.clone().downcast::<ei::Pointer>() {
                                debug!("Stored ei_pointer interface");
                                pending.pointer = Some(pointer);
                            }
                        }
                        "ei_pointer_absolute" => {
                            if let Some(pointer_abs) =
                                object.clone().downcast::<ei::PointerAbsolute>()
                            {
                                debug!("Stored ei_pointer_absolute interface");
                                pending.pointer_absolute = Some(pointer_abs);
                            }
                        }
                        "ei_keyboard" => {
                            if let Some(keyboard) = object.clone().downcast::<ei::Keyboard>() {
                                debug!("Stored ei_keyboard interface");
                                pending.keyboard = Some(keyboard);
                            }
                        }
                        "ei_button" => {
                            if let Some(button) = object.clone().downcast::<ei::Button>() {
                                debug!("Stored ei_button interface");
                                pending.button = Some(button);
                            }
                        }
                        "ei_scroll" => {
                            if let Some(scroll) = object.clone().downcast::<ei::Scroll>() {
                                debug!("Stored ei_scroll interface");
                                pending.scroll = Some(scroll);
                            }
                        }
                        _ => {
                            debug!("Ignoring interface: {}", interface_name);
                        }
                    }
                }
            }
            ei::device::Event::Done => {
                debug!(
                    "✓ Device configuration complete (pending_devices count: {})",
                    self.pending_devices.len()
                );
                // Move the first (current) pending device to active devices
                // Since devices are configured one at a time, take the first one
                let pending_opt = if let Some((&id, _)) = self.pending_devices.iter().next() {
                    self.pending_devices.remove(&id)
                } else {
                    None
                };

                if let Some(pending) = pending_opt {
                    debug!("  Found pending device, checking name...");
                    if let Some(name) = pending.name {
                        debug!("  Device has name: '{}'", name);
                        let device_info = DeviceInfo {
                            device: pending.device,
                            name: name.clone(),
                            pointer: pending.pointer,
                            pointer_absolute: pending.pointer_absolute,
                            keyboard: pending.keyboard,
                            button: pending.button,
                            scroll: pending.scroll,
                            regions: pending.regions.clone(),
                            paused: true, // Devices start paused
                            keymap: pending.keymap.clone(),
                        };

                        debug!(
                            "✓ Device '{}' ready with interfaces: pointer={}, pointer_absolute={}, keyboard={}, button={}, scroll={}, regions={}, paused={}",
                            name,
                            device_info.pointer.is_some(),
                            device_info.pointer_absolute.is_some(),
                            device_info.keyboard.is_some(),
                            device_info.button.is_some(),
                            device_info.scroll.is_some(),
                            device_info.regions.len(),
                            device_info.paused
                        );

                        // Log region details
                        for (idx, region) in device_info.regions.iter().enumerate() {
                            debug!(
                                "  Region {}: offset=({}, {}), size={}x{}, scale={}",
                                idx, region.x, region.y, region.width, region.height, region.scale
                            );
                        }

                        self.devices.push(device_info.clone());
                        debug!(
                            "✓ Device '{}' added to active devices list (total devices: {})",
                            name,
                            self.devices.len()
                        );
                    } else {
                        warn!("  Device completed without name, discarding! pending.name was None");
                    }
                } else {
                    warn!(
                        "  Device::Done received but no pending devices found (pending count: {})",
                        self.pending_devices.len()
                    );
                }
            }
            ei::device::Event::Resumed { serial } => {
                debug!("Device resumed with serial {}", serial);
                // Resume the most recently added device (or all if unsure)
                // In practice, the most recently added device is the one being resumed
                if let Some(device_info) = self.devices.last_mut() {
                    device_info.paused = false;
                    debug!(
                        "✓ Device '{}' is now RESUMED and ready for input",
                        device_info.name
                    );
                }
            }
            ei::device::Event::Paused { serial } => {
                debug!("Device paused with serial {}", serial);
                // Pause the most recently active device
                if let Some(device_info) = self.devices.last_mut() {
                    device_info.paused = true;
                    debug!("Device '{}' is now PAUSED", device_info.name);
                }
            }
            ei::device::Event::Destroyed { serial } => {
                debug!("Device destroyed with serial {}", serial);
                // Remove from pending devices (if any)
                self.pending_devices.clear();
                // Remove the last device (most recently added)
                if !self.devices.is_empty() {
                    let removed = self.devices.pop();
                    if let Some(dev) = removed {
                        debug!("Removed device '{}' from active devices", dev.name);
                    }
                }
            }
            _ => {
                debug!("Received other device event");
            }
        }

        Ok(())
    }

    /// Handle keyboard events to extract keymap information
    fn handle_keyboard_event(
        &mut self,
        _keyboard: ei::Keyboard,
        event: ei::keyboard::Event,
    ) -> Result<()> {
        match event {
            ei::keyboard::Event::Keymap {
                keymap_type,
                size,
                keymap,
            } => {
                debug!("Received keymap: size={}, type={:?}", size, keymap_type);

                // Get the raw fd from the keymap (it's an OwnedFd)
                use std::os::fd::AsRawFd;
                let fd = keymap.as_raw_fd();

                // Create keymap processor from the fd
                match KeymapProcessor::new(fd, keymap_type.into(), size) {
                    Ok(processor) => {
                        debug!("Keymap processor created successfully");
                        // Store the keymap in the currently pending device
                        if let Some(pending) = self.get_current_pending_device_mut() {
                            pending.keymap = Some(Arc::new(processor));
                            debug!("Keymap attached to pending device");
                        } else {
                            warn!("Received keymap but no pending device found");
                        }
                    }
                    Err(e) => {
                        warn!("Failed to create keymap processor: {}", e);
                    }
                }
            }
            _ => {
                debug!("Received other keyboard event: {:?}", event);
            }
        }

        Ok(())
    }

    /// Get the first device with keyboard capability
    fn get_keyboard_device(&self) -> Option<&DeviceInfo> {
        self.devices.iter().find(|d| d.keyboard.is_some())
    }

    /// Get the device with absolute pointer capability (for Synergy absolute motion)
    fn get_absolute_pointer_device(&self) -> Option<&DeviceInfo> {
        // Prioritize devices with pointer_absolute interface
        self.devices.iter().find(|d| d.pointer_absolute.is_some())
    }

    /// Get the first device with pointer capability (relative or absolute)
    fn get_pointer_device(&self) -> Option<&DeviceInfo> {
        self.devices
            .iter()
            .find(|d| d.pointer.is_some() || d.pointer_absolute.is_some())
    }

    /// Check if we have at least one resumed keyboard device
    pub fn has_keyboard(&self) -> bool {
        self.devices
            .iter()
            .any(|d| !d.paused && d.keyboard.is_some())
    }

    /// Check if we have at least one resumed pointer device (relative or absolute)
    pub fn has_pointer(&self) -> bool {
        self.devices
            .iter()
            .any(|d| !d.paused && (d.pointer.is_some() || d.pointer_absolute.is_some()))
    }

    /// Get screen dimensions from all device regions
    ///
    /// Returns (x, y, width, height) representing the bounding box of all regions
    pub fn get_screen_dimensions(&self) -> (u16, u16, u16, u16) {
        if self.devices.is_empty() {
            // Default dimensions if no devices yet
            return (0, 0, 1920, 1080);
        }

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0u32;
        let mut max_y = 0u32;

        for device in &self.devices {
            for region in &device.regions {
                min_x = min_x.min(region.x);
                min_y = min_y.min(region.y);
                max_x = max_x.max(region.x + region.width);
                max_y = max_y.max(region.y + region.height);
            }
        }

        // If no regions found, use defaults
        if min_x == u32::MAX {
            return (0, 0, 1920, 1080);
        }

        let width = (max_x - min_x).min(u16::MAX as u32) as u16;
        let height = (max_y - min_y).min(u16::MAX as u32) as u16;

        (min_x as u16, min_y as u16, width, height)
    }

    /// Get current timestamp in microseconds (CLOCK_MONOTONIC equivalent)
    fn get_timestamp_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64
    }

    /// Send a frame event to mark the end of a logical event group
    fn send_frame(&mut self, device: &ei::Device) -> Result<()> {
        let timestamp = Self::get_timestamp_us();
        device.frame(0, timestamp);
        self.context.flush()?;
        Ok(())
    }

    /// Receive an event from the EI context
    ///
    /// This method polls for events from the libei connection.
    ///
    /// # Returns
    ///
    /// Returns an EI event or an error
    pub async fn recv_event(&mut self) -> Result<Option<ei::Event>> {
        // Process events which may include seat binding
        self.process_events().await?;
        Ok(None) // Events are handled internally now
    }

    /// Send events by flushing the context
    fn flush(&mut self) -> Result<()> {
        self.context.flush()?;
        Ok(())
    }
}

/// Connect to libei using a file descriptor from the RemoteDesktop portal
///
/// This function establishes a libei connection using the file descriptor
/// obtained from the RemoteDesktop portal.
///
/// # Arguments
///
/// * `fd` - The file descriptor from the RemoteDesktop portal
///
/// # Returns
///
/// Returns an initialized `Context` object ready for sending input events
///
/// # Errors
///
/// Returns an error if the connection or initialization fails
pub async fn connect_with_fd(fd: std::os::fd::RawFd) -> Result<Context> {
    debug!("Connecting to libei with fd {}", fd);

    // Convert RawFd to UnixStream
    let stream = unsafe { UnixStream::from_raw_fd(fd) };

    let context = ei::Context::new(stream)?;

    let mut ei_context = Context::new(context);

    // Perform handshake
    ei_context.handshake().await?;

    // Process initial events to get seats
    ei_context.process_events().await?;

    debug!("libei connection established");
    Ok(ei_context)
}

/// Handle a Synergy protocol message and convert it to EI events
///
/// This function takes a Synergy protocol message and converts it to the
/// corresponding libei input event.
///
/// # Arguments
///
/// * `context` - The EI context to send events through
/// * `message` - The Synergy protocol message to handle
///
/// # Errors
///
/// Returns an error if sending the EI event fails
///
/// # Note
///
/// This is a simplified implementation. A full implementation would need to:
/// - Get device objects from the bound seat
/// - Send actual input events through the device interfaces
pub async fn handle_synergy_message(context: &mut Context, message: Message) -> Result<()> {
    // First process any pending events
    context.process_events().await?;

    match message {
        // Screen enter/leave events
        Message::CursorEntered(msg) => {
            info!(
                "Cursor entered screen at ({}, {}), sequence={}",
                msg.x, msg.y, msg.sequence
            );

            if context.devices.is_empty() {
                warn!("No devices available when cursor entered! Cannot start emulating.");
                return Ok(());
            }

            // Start emulating on all devices
            debug!("Starting emulation on {} device(s)", context.devices.len());
            for device_info in &context.devices {
                debug!(
                    "  - Starting emulation on device '{}' (paused={})",
                    device_info.name, device_info.paused
                );
                // Pass 0 for last_serial and the message sequence number
                device_info.device.start_emulating(0, msg.sequence);
            }
            context.flush()?;
            debug!("Emulation started, ready to receive input events");
        }
        Message::CursorLeft(_) => {
            info!("Cursor left screen");

            // Stop emulating on all devices
            for device_info in &context.devices {
                debug!("Stopping emulation on device '{}'", device_info.name);
                device_info.device.stop_emulating(0);
            }
            context.flush()?;
        }

        // Mouse events
        Message::MouseMove(msg) => {
            debug!("Received MouseMove event: x={}, y={}", msg.x, msg.y);

            if context.devices.is_empty() {
                warn!("No devices available, discarding MouseMove event");
                return Ok(());
            }

            // Synergy sends absolute coordinates, so we need a device with pointer_absolute interface
            if let Some(device) = context.get_absolute_pointer_device() {
                if device.paused {
                    warn!(
                        "Absolute pointer device '{}' is PAUSED, cannot send mouse move",
                        device.name
                    );
                    return Ok(());
                }

                // Clone what we need before calling send_frame
                let device_clone = device.device.clone();
                let pointer_abs = device.pointer_absolute.clone();

                // Use the pointer_absolute interface for Synergy's absolute coordinates
                if let Some(pointer_abs) = pointer_abs {
                    debug!(
                        "→ EI: motion_absolute({}, {}) via device '{}'",
                        msg.x, msg.y, device.name
                    );

                    // Send absolute motion event
                    pointer_abs.motion_absolute(msg.x as f32, msg.y as f32);

                    // Send the frame to commit the event
                    context.send_frame(&device_clone)?;
                    debug!("Motion event sent and flushed");
                } else {
                    warn!(
                        "Device '{}' selected but has no pointer_absolute interface!",
                        device.name
                    );
                }
            } else {
                warn!(
                    "No device with pointer_absolute interface available for absolute mouse motion"
                );
            }
        }
        Message::MouseButtonDown(msg) => {
            if let Some(device) = context.get_pointer_device() {
                if device.paused {
                    warn!(
                        "Pointer device '{}' is PAUSED, cannot send button down",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending pointer button down: button={} using device '{}'",
                    msg.button, device.name
                );

                let device_clone = device.device.clone();
                let button_iface = device.button.clone();

                if let Some(button) = button_iface {
                    // Synergy uses: 1=left, 2=middle, 3=right
                    // Convert to Linux button codes: BTN_LEFT=0x110, BTN_MIDDLE=0x112, BTN_RIGHT=0x111
                    let button_code = match msg.button {
                        1 => 0x110, // Left
                        2 => 0x112, // Middle
                        3 => 0x111, // Right
                        _ => msg.button as u32,
                    };
                    button.button(button_code, ei::button::ButtonState::Press);
                    context.send_frame(&device_clone)?;
                } else {
                    warn!("Pointer device has no button interface");
                }
            } else {
                debug!("No pointer device available for button down");
            }
        }
        Message::MouseButtonUp(msg) => {
            if let Some(device) = context.get_pointer_device() {
                if device.paused {
                    warn!(
                        "Pointer device '{}' is PAUSED, cannot send button up",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending pointer button up: button={} using device '{}'",
                    msg.button, device.name
                );

                let device_clone = device.device.clone();
                let button_iface = device.button.clone();

                if let Some(button) = button_iface {
                    let button_code = match msg.button {
                        1 => 0x110, // Left
                        2 => 0x112, // Middle
                        3 => 0x111, // Right
                        _ => msg.button as u32,
                    };
                    button.button(button_code, ei::button::ButtonState::Released);
                    context.send_frame(&device_clone)?;
                } else {
                    warn!("Pointer device has no button interface");
                }
            } else {
                debug!("No pointer device available for button up");
            }
        }
        Message::MouseWheel(msg) => {
            if let Some(device) = context.get_pointer_device() {
                if device.paused {
                    warn!(
                        "Pointer device '{}' is PAUSED, cannot send scroll",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending scroll: ydelta={} using device '{}'",
                    msg.ydelta, device.name
                );

                let device_clone = device.device.clone();
                let scroll_iface = device.scroll.clone();

                if let Some(scroll) = scroll_iface {
                    // Synergy sends wheel delta in some unit, convert to appropriate scroll amount
                    // Positive ydelta means scroll down, negative means scroll up
                    let scroll_y = msg.ydelta as f32;
                    scroll.scroll(0.0, scroll_y);
                    context.send_frame(&device_clone)?;
                } else {
                    warn!("Pointer device has no scroll interface");
                }
            } else {
                debug!("No pointer device available for scroll");
            }
        }

        // Keyboard events
        Message::KeyDown(msg) => {
            if let Some(device) = context.get_keyboard_device() {
                if device.paused {
                    warn!(
                        "Keyboard device '{}' is PAUSED, cannot send key down",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending key down: keysym=0x{:x} using device '{}'",
                    msg.keyid, device.name
                );

                let device_clone = device.device.clone();
                let keyboard_iface = device.keyboard.clone();
                let keymap_opt = device.keymap.clone();

                if let Some(keyboard) = keyboard_iface {
                    // Synergy sends X11 keysyms, convert to Linux evdev keycodes using keymap
                    if let Some(keymap) = keymap_opt {
                        // Translate Synergy's modifier keysyms to standard X11 keysyms
                        let keysym = translate_synergy_keysym(msg.keyid as u32);
                        if let Some(combination) = keymap.lookup_keysym(keysym) {
                            debug!(
                                "  Mapped keysym 0x{:x} (→0x{:x}) to keycode {} (modifiers: 0x{:x})",
                                msg.keyid, keysym, combination.keycode, combination.modifiers
                            );

                            // Send the key press
                            // Note: Synergy sends modifier keys as separate events, so we just
                            // translate the keysym to its base keycode without synthesizing modifiers
                            // Subtract 8 to convert X11 keycode to evdev keycode
                            keyboard.key(combination.keycode - 8, ei::keyboard::KeyState::Press);
                            context.send_frame(&device_clone)?;
                        } else {
                            warn!(
                                "No keycode mapping found for keysym 0x{:x} (→0x{:x})",
                                msg.keyid, keysym
                            );
                        }
                    } else {
                        warn!(
                            "No keymap available, cannot translate keysym 0x{:x}",
                            msg.keyid
                        );
                    }
                } else {
                    warn!("Keyboard device has no keyboard interface");
                }
            } else {
                debug!("No keyboard device available for key down");
            }
        }
        Message::KeyDownWithLanguage(msg) => {
            if let Some(device) = context.get_keyboard_device() {
                if device.paused {
                    warn!(
                        "Keyboard device '{}' is PAUSED, cannot send key down with language",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending key down (with language): keysym=0x{:x} using device '{}'",
                    msg.keyid, device.name
                );

                let device_clone = device.device.clone();
                let keyboard_iface = device.keyboard.clone();
                let keymap_opt = device.keymap.clone();

                if let Some(keyboard) = keyboard_iface {
                    if let Some(keymap) = keymap_opt {
                        // Translate Synergy's modifier keysyms to standard X11 keysyms
                        let keysym = translate_synergy_keysym(msg.keyid as u32);
                        if let Some(combination) = keymap.lookup_keysym(keysym) {
                            debug!(
                                "  Mapped keysym 0x{:x} (→0x{:x}) to keycode {} (modifiers: 0x{:x})",
                                msg.keyid, keysym, combination.keycode, combination.modifiers
                            );
                            // Subtract 8 to convert X11 keycode to evdev keycode
                            keyboard.key(combination.keycode - 8, ei::keyboard::KeyState::Press);
                            context.send_frame(&device_clone)?;
                        } else {
                            warn!(
                                "No keycode mapping found for keysym 0x{:x} (→0x{:x})",
                                msg.keyid, keysym
                            );
                        }
                    } else {
                        warn!(
                            "No keymap available, cannot translate keysym 0x{:x}",
                            msg.keyid
                        );
                    }
                } else {
                    warn!("Keyboard device has no keyboard interface");
                }
            } else {
                debug!("No keyboard device available for key down");
            }
        }
        Message::KeyUp(msg) => {
            if let Some(device) = context.get_keyboard_device() {
                if device.paused {
                    warn!(
                        "Keyboard device '{}' is PAUSED, cannot send key up",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending key up: keysym=0x{:x} using device '{}'",
                    msg.keyid, device.name
                );

                let device_clone = device.device.clone();
                let keyboard_iface = device.keyboard.clone();
                let keymap_opt = device.keymap.clone();

                if let Some(keyboard) = keyboard_iface {
                    if let Some(keymap) = keymap_opt {
                        // Translate Synergy's modifier keysyms to standard X11 keysyms
                        let keysym = translate_synergy_keysym(msg.keyid as u32);
                        if let Some(combination) = keymap.lookup_keysym(keysym) {
                            debug!(
                                "  Mapped keysym 0x{:x} (→0x{:x}) to keycode {} (modifiers: 0x{:x})",
                                msg.keyid, keysym, combination.keycode, combination.modifiers
                            );
                            // Subtract 8 to convert X11 keycode to evdev keycode
                            keyboard.key(combination.keycode - 8, ei::keyboard::KeyState::Released);
                            context.send_frame(&device_clone)?;
                        } else {
                            warn!(
                                "No keycode mapping found for keysym 0x{:x} (→0x{:x})",
                                msg.keyid, keysym
                            );
                        }
                    } else {
                        warn!(
                            "No keymap available, cannot translate keysym 0x{:x}",
                            msg.keyid
                        );
                    }
                } else {
                    warn!("Keyboard device has no keyboard interface");
                }
            } else {
                debug!("No keyboard device available for key up");
            }
        }
        Message::KeyRepeat(msg) => {
            if let Some(device) = context.get_keyboard_device() {
                if device.paused {
                    warn!(
                        "Keyboard device '{}' is PAUSED, cannot send key repeat",
                        device.name
                    );
                    return Ok(());
                }

                debug!(
                    "Sending key repeat: keysym=0x{:x} using device '{}'",
                    msg.keyid, device.name
                );

                let device_clone = device.device.clone();
                let keyboard_iface = device.keyboard.clone();
                let keymap_opt = device.keymap.clone();

                if let Some(keyboard) = keyboard_iface {
                    if let Some(keymap) = keymap_opt {
                        // Translate Synergy's modifier keysyms to standard X11 keysyms
                        let keysym = translate_synergy_keysym(msg.keyid as u32);
                        if let Some(combination) = keymap.lookup_keysym(keysym) {
                            debug!(
                                "  Mapped keysym 0x{:x} (→0x{:x}) to keycode {} (modifiers: 0x{:x})",
                                msg.keyid, keysym, combination.keycode, combination.modifiers
                            );
                            // Key repeat is simulated by sending press again
                            // Subtract 8 to convert X11 keycode to evdev keycode
                            keyboard.key(combination.keycode - 8, ei::keyboard::KeyState::Press);
                            context.send_frame(&device_clone)?;
                        } else {
                            warn!(
                                "No keycode mapping found for keysym 0x{:x} (→0x{:x})",
                                msg.keyid, keysym
                            );
                        }
                    } else {
                        warn!(
                            "No keymap available, cannot translate keysym 0x{:x}",
                            msg.keyid
                        );
                    }
                } else {
                    warn!("Keyboard device has no keyboard interface");
                }
            } else {
                debug!("No keyboard device available for key repeat");
            }
        }

        // Other messages
        Message::KeepAlive(_) => {
            debug!("Received keepalive from Synergy server");
        }
        _ => {
            debug!("Unhandled Synergy message: {:?}", message);
        }
    }

    Ok(())
}
