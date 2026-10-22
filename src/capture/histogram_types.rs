use anyhow::{Result, anyhow};

// -----------------------------------------------------------------------------
// webOS 25 Histogram ABI (reverse-engineered from libvtcapture.so.1) and the
// direct V4L2 control definitions shared by the probe and the backend
// -----------------------------------------------------------------------------

/// Shared library exposing the histogram capture API on webOS 25 (2025+ models).
pub const LIBVTCAPTURE_PATH: &str = "/usr/lib/libvtcapture.so.1";

/// Zone counts for the four native histogram presets: [16, 16, 16, 14].
pub const ZONES_PER_PRESET: [usize; 4] = [16, 16, 16, 14];

/// Presumed zone arrangement within a preset; row-major hypothesis, to be
/// validated on-device before it is used for light placement.
pub const ZONE_GRID: (u32, u32) = (4, 4);

/// Per-zone channel payload: three 512-byte channel blocks.
pub const CHANNEL_BYTES: usize = 3 * 512;

/// Native per-entry stride observed in the histogram dump:
/// 4-byte zone index followed by [`CHANNEL_BYTES`] of channel data.
pub const ENTRY_SIZE: usize = std::mem::size_of::<VtHistogramEntry>();

/// One histogram zone as laid out by the native dump.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VtHistogramEntry {
    /// Zone index reported by the driver; must equal the entry's position in the buffer.
    pub index: i32,
    /// Per-zone channel payload.
    pub channels: [u8; CHANNEL_BYTES],
}

// -----------------------------------------------------------------------------
// Direct V4L2 histogram controls (kernel node, webOS 25)
//
// Reverse-engineered from libvtcapture's VtHistogram and confirmed live on
// device by the C5 probe campaign (analysis/probe-results.md): the kernel
// serves these controls to a plain root process, including during DRM
// playback. Single shared definition; the probe and the production backend
// both import from here.
// -----------------------------------------------------------------------------

/// Kernel node serving the histogram controls directly (webOS 25).
pub(crate) const HISTOGRAM_V4L2_DEVICE: &str = "/dev/video60";

/// V4L2 control IDs reverse-engineered from libvtcapture's VtHistogram
/// (V4L2_CTRL_CLASS_USER, 0x0098 << 16 | 0x2a1x).
pub(crate) const CTRL_HISTOGRAM_ENABLE: u32 = 0x0098_2a14;
pub(crate) const CTRL_HISTOGRAM_PRESET: u32 = 0x0098_2a15;
pub(crate) const CTRL_HISTOGRAM_DATA: u32 = 0x0098_2a16;

/// `VIDIOC_S_EXT_CTRLS` as issued by the TV's 32-bit ARM userspace
/// (0x18 = sizeof v4l2_ext_controls on that ABI).
pub(crate) const VIDIOC_S_EXT_CTRLS: libc::c_ulong = 0xc018_5648;
/// `VIDIOC_G_EXT_CTRLS`, same ABI.
pub(crate) const VIDIOC_G_EXT_CTRLS: libc::c_ulong = 0xc018_5647;

/// `struct v4l2_ext_control` exactly as the kernel UAPI lays it out for the
/// TV's 32-bit ARM ABI: id@0, size@4, reserved2@8, then the 8-byte-wide
/// packed union at @12 (`value64`/pointers make it 8 bytes; this code only
/// uses the union's 4-byte members — a raw value for the value controls, or
/// a buffer pointer for the payload control) with `reserved3` as the union's
/// tail, keeping the 20-byte packed stride. Fields are stored as fixed-width
/// `u32` so the layout is identical on 64-bit hosts, where the size unit
/// tests run. Pointer payloads are cast `as usize as u32`, lossless on the
/// ARM32 target that is the only caller reaching the ioctl.
#[repr(C)]
pub(crate) struct V4l2ExtControl {
    pub(crate) id: u32,
    pub(crate) size: u32,
    pub(crate) reserved2: u32,
    pub(crate) value_or_ptr: u32,
    pub(crate) reserved3: u32,
}

/// `struct v4l2_ext_controls` (24 bytes, 32-bit ARM ABI; `controls` sits at
/// byte 20 and uses the same fixed-width pointer storage as
/// [`V4l2ExtControl`]).
#[repr(C)]
pub(crate) struct V4l2ExtControls {
    pub(crate) which: u32,
    pub(crate) count: u32,
    pub(crate) error_idx: u32,
    pub(crate) request_fd: i32,
    pub(crate) reserved: u32,
    pub(crate) controls: u32,
}

/// Builds the single-control `VIDIOC_*_EXT_CTRLS` wrapper: old-style USER
/// class `which`, exactly one control, no request API, `controls` pointing at
/// `ctrl`.
pub(crate) fn build_ext_controls(ctrl: &mut V4l2ExtControl) -> V4l2ExtControls {
    V4l2ExtControls {
        which: 0x0098_0000,
        count: 1,
        error_idx: 0,
        request_fd: -1,
        reserved: 0,
        controls: ctrl as *mut V4l2ExtControl as usize as u32,
    }
}

// -----------------------------------------------------------------------------
// Histogram Buffer
// -----------------------------------------------------------------------------

/// Host-side staging buffer whose layout matches the native histogram dump,
/// safe to hand to the driver as a `*mut VtHistogramEntry` array.
pub struct HistogramBuffer {
    entries: Vec<VtHistogramEntry>,
}

impl HistogramBuffer {
    /// Allocates a zeroed buffer with `zones` entries.
    pub fn new(zones: usize) -> Self {
        Self {
            entries: vec![
                VtHistogramEntry {
                    index: 0,
                    channels: [0; CHANNEL_BYTES],
                };
                zones
            ],
        }
    }

    /// Resets every entry to the pre-fill baseline: the driver's expected
    /// position in `index` and an all-zero channel payload. The buffer is
    /// reused across frames, so a partial or failed fill must not leave
    /// stale channel bytes behind — they would masquerade as fresh data and
    /// `is_wholly_absent` could never fire again after the first good frame.
    pub fn prefill_indices(&mut self) {
        for (position, entry) in self.entries.iter_mut().enumerate() {
            entry.index = position as i32;
            entry.channels = [0; CHANNEL_BYTES];
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
    /// driver that shuffles zones or writes foreign indices. It cannot catch a
    /// short write: untouched entries keep their prefilled index.
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
        assert_eq!(std::mem::offset_of!(VtHistogramEntry, channels), 4);
    }

    // -----------------------------------------------------------------------------
    // Direct V4L2 control ABI: exact layout is non-negotiable
    // -----------------------------------------------------------------------------

    #[test]
    fn ext_ctrl_struct_sizes_match_32bit_arm_abi() {
        assert_eq!(std::mem::size_of::<V4l2ExtControl>(), 20);
        assert_eq!(std::mem::size_of::<V4l2ExtControls>(), 24);
    }

    #[test]
    fn ext_ctrl_field_offsets_match_kernel_packed_union() {
        // The kernel reads the ext_control union at byte 12: a payload pointer
        // placed at any other offset is read as NULL (EFAULT) or garbage.
        let mut ctrl = V4l2ExtControl {
            id: 1,
            size: 2,
            reserved2: 3,
            value_or_ptr: 4,
            reserved3: 5,
        };
        let base = std::ptr::addr_of!(ctrl) as usize;
        assert_eq!(
            std::ptr::addr_of!(ctrl.value_or_ptr) as usize - base,
            12,
            "value/pointer union member must sit at the kernel's packed offset 12"
        );

        let ctrls = build_ext_controls(&mut ctrl);
        let wrapper_base = std::ptr::addr_of!(ctrls) as usize;
        assert_eq!(
            std::ptr::addr_of!(ctrls.controls) as usize - wrapper_base,
            20,
            "controls pointer must sit at the kernel's offset 20"
        );
    }

    #[test]
    fn build_ext_controls_wraps_single_user_class_control() {
        let mut ctrl = V4l2ExtControl {
            id: CTRL_HISTOGRAM_DATA,
            size: (ZONES_PER_PRESET[0] * ENTRY_SIZE) as u32,
            reserved2: 0,
            value_or_ptr: 0,
            reserved3: 0,
        };
        // The payload control requests all 16 preset-0 zones in one ioctl.
        assert_eq!(ctrl.size, 0x6040);
        let ctrls = build_ext_controls(&mut ctrl);
        assert_eq!(ctrls.which, 0x0098_0000);
        assert_eq!(ctrls.count, 1);
        assert_eq!(ctrls.error_idx, 0);
        assert_eq!(ctrls.request_fd, -1);
        assert_eq!(ctrls.reserved, 0);
        // On the 32-bit ARM TV this is full pointer identity; on a 64-bit host
        // the fixed-width `controls` field carries the low 32 bits.
        assert_eq!(
            ctrls.controls as usize,
            std::ptr::addr_of_mut!(ctrl) as usize & u32::MAX as usize,
            "wrapper must carry the control's 32-bit ARM address"
        );
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

    #[test]
    fn prefill_indices_resets_a_dirty_buffer_to_the_baseline() {
        let mut buffer = HistogramBuffer::new(ZONES_PER_PRESET[0]);
        // Dirty the buffer the way a previous frame's fill would leave it:
        // stale channel bytes and a clobbered index.
        buffer.entries[0].channels[0] = 0xFF;
        buffer.entries[15].channels[1535] = 7;
        buffer.entries[3].index = 99;

        buffer.prefill_indices();

        for i in 0..buffer.zones() {
            let entry = buffer.entry(i);
            assert_eq!(entry.index, i as i32, "indices must be sequential");
            assert!(
                entry.channels.iter().all(|&byte| byte == 0),
                "entry {i} channels must be fully zeroed"
            );
        }
        assert!(
            buffer.is_wholly_absent(),
            "prefill must restore the wholly-absent baseline"
        );
    }
}
