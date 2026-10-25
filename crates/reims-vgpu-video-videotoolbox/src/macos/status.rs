//! VideoToolbox's statuses as core's backend errors. Apple's host sends the
//! guest its codec's own status for `esta` and `efrm`, and the guest's
//! VideoToolbox is the same framework as this host's, so every status a
//! VideoToolbox, CoreMedia or CoreVideo call answers goes to the guest as it is
//! (`BackendError::Native`). What this crate refuses itself, before any call,
//! keeps core's three kinds.

use reims_vgpu_video_core::backend::BackendError;

use super::ffi::core_foundation::OSStatus;
use super::ffi::video_toolbox::{
    VTDecodeInfoFlags, kVTDecodeInfo_SkippedLeadingFrameDropped, kVTVideoDecoderReferenceMissingErr,
};

/// What a VideoToolbox, CoreMedia or CoreVideo call's status means for
/// core: the status itself. `call` names it for the log and the error's
/// text.
pub(crate) fn native_error(call: &str, status: OSStatus) -> BackendError {
    BackendError::Native {
        status,
        message: format!("{call} answered {status}"),
    }
}

/// What a frame's status in the decoder's output callback means for core:
/// the status itself, but for a leading picture the decoder skipped. Apple's
/// host forwards that one with the callback's flags, FrameDropped and
/// SkippedLeadingFrameDropped, and its reply after the emit, which core does
/// for `SkippedLeadingPicture`.
pub(crate) fn output_error(status: OSStatus, flags: VTDecodeInfoFlags) -> BackendError {
    if status == kVTVideoDecoderReferenceMissingErr
        && flags & kVTDecodeInfo_SkippedLeadingFrameDropped != 0
    {
        return BackendError::SkippedLeadingPicture(format!(
            "the decoder's output answered {status}, a leading picture dropped"
        ));
    }
    native_error("the decoder's output", status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reims_vgpu_video_core::status::Status;

    /// Each status reaches the guest as VideoToolbox answered it, as from
    /// Apple's host, where core's kinds would have made -17694
    /// (kVTVideoDecoderReferenceMissingErr) bad data and -12903 a
    /// malfunction.
    #[test]
    fn statuses_reach_the_guest_as_videotoolbox_answered_them() {
        for status in [-12909, -17694, -12906, -12911, -12903] {
            assert_eq!(native_error("decode", status).status(), status);
        }
        assert!(matches!(
            native_error("set", -12900),
            BackendError::Native { status: -12900, .. }
        ));
        assert_eq!(
            native_error("decode", 0).status(),
            Status::VideoDecoderMalfunction.code()
        );
    }

    /// A leading picture the decoder skipped is core's to emit as Apple's
    /// host does; the same status without the flag is VideoToolbox's own.
    #[test]
    fn a_skipped_leading_picture_is_cores_to_emit() {
        let skipped = output_error(-17694, 0xa);
        assert!(matches!(skipped, BackendError::SkippedLeadingPicture(_)));
        assert_eq!(skipped.status(), -17694);
        assert!(matches!(
            output_error(-17694, 0x2),
            BackendError::Native { status: -17694, .. }
        ));
        assert!(matches!(
            output_error(-12909, 0xa),
            BackendError::Native { status: -12909, .. }
        ));
    }
}
