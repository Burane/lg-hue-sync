use anyhow::{anyhow, Result};

// -----------------------------------------------------------------------------
// webOS 25 Histogram Probe ABI (reverse-engineered from libvtcapture.so.1)
// -----------------------------------------------------------------------------

/// Shared library exposing the histogram capture API on webOS 25 (2025+ models).
#[allow(dead_code)]
pub const LIBVTCAPTURE_PATH: &str = "/usr/lib/libvtcapture.so.1";

/// Zone counts for the four native histogram presets: [16, 16, 16, 14].
#[allow(dead_code)]
pub const ZONES_PER_PRESET: [usize; 4] = [16, 16, 16, 14];

/// Presumed zone arrangement within a preset; row-major hypothesis, to be
/// validated on-device before it is used for light placement.
#[allow(dead_code)]
pub const ZONE_GRID: (u32, u32) = (4, 4);

/// Native per-entry stride observed in the histogram dump:
/// 4-byte zone index followed by 1536 bytes of channel data.
#[allow(dead_code)]
pub const ENTRY_SIZE: usize = 0x604;

/// One histogram zone as laid out by the native dump.
#[repr(C)]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtHistogramEntry {
    /// Zone index reported by the driver; must equal the entry's position in the buffer.
    pub index: i32,
    /// Per-zone channel payload (1536 bytes).
    pub channels: [u8; 1536],
}

// -----------------------------------------------------------------------------
// Histogram Buffer
// -----------------------------------------------------------------------------

/// Host-side staging buffer whose layout matches the native histogram dump,
/// safe to hand to the driver as a `*mut VtHistogramEntry` array.
#[allow(dead_code)]
pub struct HistogramBuffer {
    entries: Vec<VtHistogramEntry>,
}

#[allow(dead_code)]
impl HistogramBuffer {
    /// Allocates a zeroed buffer with `zones` entries.
    pub fn new(zones: usize) -> Self {
        Self {
            entries: vec![
                VtHistogramEntry {
                    index: 0,
                    channels: [0; 1536],
                };
                zones
            ],
        }
    }

    /// Writes each entry's position into its `index` field, establishing the
    /// expected baseline before the driver fills the buffer.
    pub fn prefill_indices(&mut self) {
        for (position, entry) in self.entries.iter_mut().enumerate() {
            entry.index = position as i32;
        }
    }

    /// Raw pointer to the entry array, for passing across the FFI boundary.
    pub fn as_mut_ptr(&mut self) -> *mut VtHistogramEntry {
        self.entries.as_mut_ptr()
    }

    /// Number of zones in the buffer.
    pub fn zones(&self) -> usize {
        self.entries.len()
    }

    /// Borrows the entry at position `i`.
    ///
    /// # Panics
    /// Panics if `i` is out of range.
    pub fn entry(&self, i: usize) -> &VtHistogramEntry {
        &self.entries[i]
    }

    /// True only when every entry's channel payload is all zero, i.e. the
    /// driver produced no histogram data at all.
    pub fn is_wholly_absent(&self) -> bool {
        self.entries
            .iter()
            .all(|entry| entry.channels.iter().all(|&byte| byte == 0))
    }

    /// Checks that every entry's `index` still equals its position, catching a
    /// driver that writes fewer zones than requested or shuffles them.
    pub fn validate_indices(&self) -> Result<()> {
        for (position, entry) in self.entries.iter().enumerate() {
            if entry.index != position as i32 {
                return Err(anyhow!(
                    "histogram entry index mismatch at position {position}: expected {position}, found {}",
                    entry.index
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------------
    // Reverse-engineered ABI (webOS 25): exact layout is non-negotiable
    // -----------------------------------------------------------------------------

    #[test]
    fn entry_layout_matches_native_abi() {
        assert_eq!(ENTRY_SIZE, 0x604);
        assert_eq!(std::mem::size_of::<VtHistogramEntry>(), 0x604);
    }

    #[test]
    fn channels_field_starts_at_byte_offset_4() {
        let entry = VtHistogramEntry {
            index: 7,
            channels: [0; 1536],
        };
        let base = std::ptr::addr_of!(entry.index) as usize;
        let channels = std::ptr::addr_of!(entry.channels) as usize;
        assert_eq!(channels - base, 4);
        assert_eq!(channels - base, std::mem::size_of::<i32>());
    }

    #[test]
    fn zone_presets_match_reverse_engineered_layout() {
        assert_eq!(LIBVTCAPTURE_PATH, "/usr/lib/libvtcapture.so.1");
        assert_eq!(ZONES_PER_PRESET[0], 16);
        assert_eq!(ZONES_PER_PRESET[3], 14);
        assert_eq!(ZONE_GRID, (4, 4));
    }

    // -----------------------------------------------------------------------------
    // HistogramBuffer helpers
    // -----------------------------------------------------------------------------

    #[test]
    fn prefill_indices_numbers_every_zone_and_pointer_is_aligned() {
        let mut buffer = HistogramBuffer::new(ZONES_PER_PRESET[0]);
        assert_eq!(buffer.zones(), 16);
        buffer.prefill_indices();
        for i in 0..buffer.zones() {
            assert_eq!(buffer.entry(i).index, i as i32);
        }
        assert_eq!(buffer.as_mut_ptr() as usize % 4, 0);
    }

    #[test]
    fn validate_indices_detects_driver_clobbered_entries() {
        // A zeroed buffer fails until prefill establishes index == position.
        let mut buffer = HistogramBuffer::new(ZONES_PER_PRESET[0]);
        assert!(buffer.validate_indices().is_err());
        buffer.prefill_indices();
        assert!(buffer.validate_indices().is_ok());

        buffer.entries[5].index = 99;
        let error = buffer.validate_indices().expect_err("mismatch must fail");
        let message = format!("{error:#}");
        assert!(
            message.contains("position 5"),
            "error must name position: {message}"
        );
        assert!(
            message.contains("99"),
            "error must name the found index: {message}"
        );
    }

    #[test]
    fn is_wholly_absent_tracks_channel_payload() {
        let mut buffer = HistogramBuffer::new(4);
        assert!(buffer.is_wholly_absent());
        buffer.entries[2].channels[1535] = 1;
        assert!(!buffer.is_wholly_absent());
    }
}
