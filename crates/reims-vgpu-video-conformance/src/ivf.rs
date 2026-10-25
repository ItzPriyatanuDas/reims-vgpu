//! Just enough of IVF, the container aomenc, SvtAv1EncApp, rav1e and
//! vpxenc write, to take out its frames: a 32-byte file header (`DKIF`, its
//! length at byte 6, the codec's FourCC at 8), then each frame as a 12-byte
//! header (the size, little-endian, and a 64-bit timestamp) and its data.
//! An AV1 IVF frame is one temporal unit in the low overhead format, a VP9
//! one a frame or a superframe.

/// Each frame's offset in `file` and size, in file order, of an AV1 file.
pub fn frames(file: &[u8]) -> Result<Vec<(usize, usize)>, String> {
    frames_of(file, b"AV01", "an AV1")
}

/// Each frame's offset in `file` and size, in file order, of a VP9 file.
pub fn vp9_frames(file: &[u8]) -> Result<Vec<(usize, usize)>, String> {
    frames_of(file, b"VP90", "a VP9")
}

/// The frames of an IVF file of the codec `fourcc` names, `what` in the
/// error otherwise.
fn frames_of(file: &[u8], fourcc: &[u8; 4], what: &str) -> Result<Vec<(usize, usize)>, String> {
    if file.get(..4) != Some(b"DKIF") {
        return Err("not an IVF file".into());
    }
    let header = file
        .get(6..8)
        .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
        .ok_or("IVF header truncated")?;
    if file.get(8..12) != Some(fourcc) {
        return Err(format!("not {what} IVF file"));
    }
    let mut frames = Vec::new();
    let mut at = header;
    while at < file.len() {
        let size = file
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
            .ok_or("IVF frame header truncated")?;
        at += 12;
        if at + size > file.len() {
            return Err(format!("IVF frame {} past the file's end", frames.len()));
        }
        frames.push((at, size));
        at += size;
    }
    Ok(frames)
}
