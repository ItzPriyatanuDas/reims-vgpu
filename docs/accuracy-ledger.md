# Accuracy ledger

A ledger, not a journal: rows are edited in place. Each row is one place where the Vulkan
platform's default trades exactness against Metal for compile time (pipeline making) or GPU
time. The fast form is the default. The accurate form stays behind its opt-in switch, and both
are kept always. An accurate form becomes the default only where it costs nothing measurable in
compile or GPU time. An accuracy difference is logged (`metal2vulkan-degrade-<code>`, first line,
then counted) and the process runs on. The process ends only where Metal itself ends it, or where
an operation cannot run at all. `REIMS_VGPU_STRICT=1` ends it at each difference instead, for
tests.

Switches: `REIMS_VGPU_EXACT=<field>[,<field>...]` sets the runtime's `DevicePolicy` field to its
exact form at device creation. The runtime's own ledger, with every bound in full, is
metal2vulkan's `docs/FAST-PATHS.md` at the pinned commit. Costs are the RTX 5080 Laptop GPU's,
cold, from that file: the making of the big glass pipelines (A2X, Xgc14, Xgc15), then GPU time
per draw.

| Fast default | Accurate switch | Bound of the fast form | Cost of the accurate form (5080) |
|---|---|---|---|
| The device's sampler (filter, footprint, level, anisotropy, sRGB, comparisons) | `sampling` | linear filter 1 code, sRGB 1.83; the rest unmeasured | Xgc15 making about 2x |
| The device's conversion of a texel's codes | `texel_conversion` (a read alone; a sample or gather with `sampling`) | 1 f32 ulp (8/16-bit), 0.18 code (5/6-bit packed) | reads: about +0.03 s making, GPU +4-18% |
| Device maths library, division, fma | `float_maths` | Vulkan's precision table (e.g. Atan 4096 ulp) against Metal's 4-6 | A2X 1.0 → 4.6 s, GPU 2x |
| Device NaN bits; device min/max/clamp | `nan_and_min_max` | NaN bits, ±0 and denormal picks | A2X 1.0 → 1.9 s, GPU +18%; its full texel rule (gathers included, but not a cube gather) adds A2X 1.85 → 2.0 s, and a first draw with a linear runtime sampler bound remakes its pipeline (about 2 s), GPU +8% |
| Device `dot`, `mix`, `fast::sqrt` | `rounding_order` | dot (3n−1)·2^-24·Σ, mix 2^-21, sqrt 4.5 ulp | about +0.05 s making |
| Device half rounding, widening and folding; a half texel's NaN | `half_rounding`; the texel NaN also under `nan_and_min_max` (no cost over that switch) | AMD and Intel (RADV, ANV) and the RTX 5080 round exactly (5080: 8192 of 8192 narrowed sums are Metal's). The RTX 3090, on the same 615.71.09 driver, and lavapipe drop the rounding: a half result keeps float precision, its differences compounding across operations. A value past half's range stays finite until a half store saturates it to infinity. Elsewhere 1 half ulp. NaN bits; half denormals may flush | A2X 1.0 → 7.2 s, GPU +40% |
| The device's blend of a safe-math half output into a blended 16-bit float attachment | `half_blend` (a `half4` output into R16/RG16/RGBA16Float, safe or relaxed math; elsewhere the device blend stays) | at the largest operand: 16-bit float 2.5 half ulps on NVIDIA (blend-colour factors), 2 on AMD, Intel and lavapipe; 32-bit float 1 ulp on NVIDIA, 2 elsewhere | making 11.1 → 24.0 ms; GPU 25 → 82 µs for one 1080p draw, 0.51 → 2.29 ms for 32 overlapping draws |
| An image over the buffer's bytes for a texture that only reads (its usage holds no `RenderTarget`, `ShaderWrite`, `ShaderAtomic` or block writes; `Unknown` holds them all) over a buffer whose rows, or an array's slices, lie with bytes between them, such as a guest browser's tiles | `padded_reads` (the runtime's shadow, copied in before each use) | the image's first move (a layout transition, which Vulkan lets read and write the whole range, and discard it from `UNDEFINED`) reaches the bytes between its rows, as for packed rows: the host reads them before the move and writes back only the bytes the move changed, and only on pages it may write (the runtime's moves changed none on the M3 Max, lavapipe, the RTX 5080 or RADV); a changed byte on a page the host may not write keeps the move's value, announced (`UnrestoredFirstMoveBytes`, which dies in strict mode); a CPU write to them between that read and the write back is undone, once per image, unlogged (the runtime's known gap), and no byte outside them is touched. Metal never writes those bytes (M3 Max, under concurrent CPU writes too) | not measured on the 5080; Iris Xe (ANV, Mesa 26.2.4) about +0.36 ms a frame (median of 15 pairs, 0.08-0.60) sampling 4 padded BGRA8 tiles (two 1307x512, 300x200, 1913x96) |
| An extended-range blend source rounded as a loaded colour | none yet: parked (the 5080 effort first; no 5080 boot reaches it) | 1 code | not measured |
| Bicubic near-tie rounding | none yet: parked (the 5080 effort first; no 5080 boot reaches it) | 1 grid step (1/4080) within 2^-21 of a tie | not measured |

All exact at once (`all`: every switch but `sampling`): A2X 1.0 → 17.5 s, Xgc14 0.9 → 5.0 s,
Xgc15 0.65 → 3.1 s, GPU about 2-2.7x, and `padded_reads`' copies.

Differences no switch can fix (device capabilities, undefined values) are not speed choices. They
are listed in the runtime's `docs/FAST-PATHS.md`, "Not speed choices", and are logged the same way.

## Logged gaps on our side (no accurate form yet; not speed choices)

Each is logged once per kind and counted, and the work runs. `REIMS_VGPU_STRICT=1` ends the
process there instead.

| What runs instead of Metal's answer | Where |
|---|---|
| A line width other than 1 draws 1 pixel wide | `reims-vgpu-vulkan` draw encoding |
| A provoking vertex with no exact rotation draws with the first vertex provoking | draw encoding |
| A heap texture of unmeasured size or layout takes the runtime's size and layout | heaps |
| A Metal-level answer for an unmeasured host release, or a format's description off macOS 26.7, comes from that release's description | device answers |
| A draw or tile dispatch in a varying-rate pass runs on the runtime's own regions | rate maps |
| A retyped YCbCr view reads as the texture itself | YCbCr views |
| A blit fill packed in another format repeats the packed texel | blits |
| A late resolve texture beside a dropped resolve leaves the colour resolve dropped | render passes |
| A texture of protection options from `newTextureWithDescriptor:` or `newSharedTextureWithDescriptor:` is made unprotected, as Metal's other creators make it, where Metal answers nil without the protected-content entitlement; its contents are not kept from the host | device textures |

