//! The kernel's count of each GPU engine's busy time for this process: the
//! `drm-engine-*` lines of `/proc/self/fdinfo` for each of its render-node
//! descriptors (amdgpu's, where the VCN's JPEG decoder is `jpeg` and its
//! video engines `dec` and `enc`), summed by engine. Empty off Linux, and
//! where a driver gives none.

use std::collections::BTreeMap;

/// Each engine's busy time so far, in ns, by its name.
pub(crate) fn busy() -> BTreeMap<String, u64> {
    let mut totals = BTreeMap::new();
    let Ok(fds) = std::fs::read_dir("/proc/self/fd") else {
        return totals;
    };
    for fd in fds.flatten() {
        let Ok(target) = std::fs::read_link(fd.path()) else {
            continue;
        };
        if !target.to_string_lossy().starts_with("/dev/dri/") {
            continue;
        }
        let info = format!("/proc/self/fdinfo/{}", fd.file_name().to_string_lossy());
        let Ok(text) = std::fs::read_to_string(info) else {
            continue;
        };
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("drm-engine-") else {
                continue;
            };
            let mut parts = rest.split_whitespace();
            let (Some(engine), Some(value)) = (parts.next(), parts.next()) else {
                continue;
            };
            if let Ok(ns) = value.parse::<u64>() {
                *totals
                    .entry(engine.trim_end_matches(':').to_owned())
                    .or_insert(0) += ns;
            }
        }
    }
    totals
}

/// What each engine spent between two counts of [`busy`], in ns.
pub(crate) fn spent(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
) -> Vec<(String, u64)> {
    after
        .iter()
        .map(|(engine, &ns)| {
            (
                engine.clone(),
                ns.saturating_sub(before.get(engine).copied().unwrap_or(0)),
            )
        })
        .collect()
}
