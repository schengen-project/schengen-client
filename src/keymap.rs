//! Keymap handling module
//!
//! This module processes XKB keymaps from EI keyboard devices and builds
//! a reverse mapping from keysyms to the key codes and modifiers needed
//! to produce them.

use anyhow::{Context, Result};
use kbvm::lookup::LookupTable;
use kbvm::xkb;
use kbvm::{GroupIndex, Keycode, ModifierMask};
use log::debug;
use std::collections::HashMap;
use std::os::fd::FromRawFd;
use std::os::unix::io::RawFd;

/// A key combination that produces a specific keysym
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombination {
    pub keycode: u32,
    pub modifiers: u32,
}

/// Keymap processor that maintains a reverse mapping from keysyms to key codes
#[derive(Debug)]
pub struct KeymapProcessor {
    // Map from keysym to all key combinations that can produce it
    reverse_map: HashMap<u32, Vec<KeyCombination>>,
}

impl KeymapProcessor {
    /// Create a new keymap processor from a keymap file descriptor
    ///
    /// # Arguments
    ///
    /// * `fd` - File descriptor for the keymap
    /// * `format` - Keymap format (should be XKB for libei)
    /// * `size` - Size of the keymap in bytes
    ///
    /// # Returns
    ///
    /// Returns a KeymapProcessor with the reverse mapping built
    pub fn new(fd: RawFd, format: u32, size: u32) -> Result<Self> {
        debug!(
            "Processing keymap: fd={}, format={}, size={}",
            fd, format, size
        );

        // Read the keymap from the file descriptor
        // IMPORTANT: We duplicate the fd so we don't close the original
        use std::io::Read;
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd == -1 {
            return Err(anyhow::anyhow!("Failed to duplicate fd for keymap"));
        }

        let mut file = unsafe { std::fs::File::from_raw_fd(dup_fd) };
        let mut keymap_bytes = Vec::with_capacity(size as usize);
        file.read_to_end(&mut keymap_bytes)
            .context("Failed to read keymap from fd")?;

        debug!("Read {} bytes from keymap fd", keymap_bytes.len());

        // Create XKB context and parse the keymap
        let context = xkb::Context::builder().build();
        let keymap = context
            .keymap_from_bytes(xkb::diagnostic::WriteToLog, None, &keymap_bytes)
            .context("Failed to parse keymap")?;

        debug!("Keymap parsed successfully");

        // Build lookup table from keymap
        let lookup_table = keymap.to_builder().build_lookup_table();

        debug!("Lookup table built, building reverse mapping...");

        // Build the reverse mapping
        let reverse_map = Self::build_reverse_map(&lookup_table)?;

        debug!("Reverse mapping built with {} keysyms", reverse_map.len());

        Ok(Self { reverse_map })
    }

    /// Build a reverse mapping from keysyms to key combinations
    ///
    /// This iterates through all keycodes and modifier combinations to determine
    /// which key combinations produce which keysyms.
    fn build_reverse_map(lookup_table: &LookupTable) -> Result<HashMap<u32, Vec<KeyCombination>>> {
        let mut reverse_map: HashMap<u32, Vec<KeyCombination>> = HashMap::new();

        // Use group 0 (default group)
        let group = GroupIndex(0);

        // Iterate through all possible keycodes (typically 8-255 for XKB)
        for keycode_raw in 8..256u32 {
            let keycode = Keycode::from_x11(keycode_raw);

            // Try all modifier combinations
            // Common modifiers: Shift, Lock, Control, Mod1 (Alt), Mod2, Mod3, Mod4 (Super), Mod5
            let modifier_combinations = [
                ModifierMask::NONE,                                               // No modifiers
                ModifierMask::SHIFT,                                              // Shift
                ModifierMask::CONTROL,                                            // Control
                ModifierMask::MOD1,                                               // Alt
                ModifierMask::MOD4,                                               // Super
                ModifierMask::SHIFT | ModifierMask::CONTROL,                      // Shift+Control
                ModifierMask::SHIFT | ModifierMask::MOD1,                         // Shift+Alt
                ModifierMask::CONTROL | ModifierMask::MOD1,                       // Control+Alt
                ModifierMask::SHIFT | ModifierMask::CONTROL | ModifierMask::MOD1, // Shift+Control+Alt
            ];

            for &mods in &modifier_combinations {
                // Look up the keysym for this keycode + modifiers combination
                let lookup = lookup_table.lookup(group, mods, keycode);

                // Get the first keysym from the lookup iterator
                if let Some(keysym_props) = lookup.into_iter().next() {
                    let keysym = keysym_props.keysym();
                    let keysym_raw = keysym.0;

                    if keysym_raw != 0 {
                        let combination = KeyCombination {
                            keycode: keycode_raw,
                            modifiers: mods.0,
                        };

                        reverse_map.entry(keysym_raw).or_default().push(combination);
                    }
                }
            }
        }

        debug!(
            "Built reverse map with {} unique keysyms",
            reverse_map.len()
        );

        // Log some common modifier keysyms to verify they're mapped
        let modifier_keysyms = [
            (0xffe1, "XK_Shift_L"),
            (0xffe2, "XK_Shift_R"),
            (0xffe3, "XK_Control_L"),
            (0xffe4, "XK_Control_R"),
            (0xffe9, "XK_Alt_L"),
            (0xffea, "XK_Alt_R"),
            (0xffeb, "XK_Super_L"),
            (0xffec, "XK_Super_R"),
            (0xffe7, "XK_Meta_L"),
            (0xffe8, "XK_Meta_R"),
        ];

        for (keysym, name) in &modifier_keysyms {
            if let Some(combos) = reverse_map.get(keysym) {
                debug!(
                    "  {} (0x{:x}) → keycode {}",
                    name, keysym, combos[0].keycode
                );
            } else {
                debug!("  {} (0x{:x}) → NOT MAPPED", name, keysym);
            }
        }

        Ok(reverse_map)
    }

    /// Look up the key combination needed to produce a keysym
    ///
    /// # Arguments
    ///
    /// * `keysym` - The keysym to look up
    ///
    /// # Returns
    ///
    /// Returns the key combination(s) that can produce this keysym.
    /// If multiple combinations exist, prefers the simplest one (fewest modifiers).
    pub fn lookup_keysym(&self, keysym: u32) -> Option<KeyCombination> {
        self.reverse_map.get(&keysym).and_then(|combinations| {
            // Prefer the combination with the fewest modifiers
            combinations
                .iter()
                .min_by_key(|combo| combo.modifiers.count_ones())
                .copied()
        })
    }
}
