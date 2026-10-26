//! What one decode asks of nvJPEG, and how the output is made of what it
//! writes: worked out from the request and the parsed stream before any
//! work on the GPU.

use std::borrow::Cow;
use std::ffi::c_int;

use reims_vgpu_video_codec::jpeg::{self as syntax, Image};
use reims_vgpu_video_core::jpeg::{
    DecodedSamples, JpegDecode, JpegError, JpegSubsampling, PixelRect, SampleView, write_samples,
};
use reims_vgpu_video_core::surfaces::PlaneDestination;

use super::convert::{self, Axis};
use super::nvjpeg;

/// The alignment of each device plane's rows. Ours.
const PITCH_ALIGNMENT: usize = 256;

/// The subsampling core numbers as CMPhoto does, of one the parser read.
fn subsampling(read: syntax::Subsampling) -> Option<JpegSubsampling> {
    Some(match read {
        syntax::Subsampling::Yuv420 => JpegSubsampling::Yuv420,
        syntax::Subsampling::Yuv422 => JpegSubsampling::Yuv422,
        syntax::Subsampling::Yuv444 => JpegSubsampling::Yuv444,
        syntax::Subsampling::Yuv411 => JpegSubsampling::Yuv411,
        syntax::Subsampling::Grey => JpegSubsampling::Grey,
        syntax::Subsampling::Yuv440 => return None,
    })
}

/// How a decode's output is made of nvJPEG's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// The components' planes into a YCbCr layout whose chroma is at
    /// `layout`'s sampling (grey's luma alone, for core to write its
    /// chroma), each copied or resampled ([`convert`]).
    Planes {
        /// The sampling the layout's chroma is at; grey for a grey stream.
        layout: JpegSubsampling,
    },
    /// nvJPEG's BGR, at scale 1, made BGRA ([`convert::write_bgra`]).
    Bgr,
    /// The components' planes, at the stream's sampling, made BGRA here
    /// ([`convert::write_ycbcr_bgra`]): at a scale, where nvJPEG's BGR is
    /// far from the reference ([`convert`]).
    Bgra,
}

impl Route {
    /// How `request`'s output is made: what Apple's hardware refuses is
    /// refused here, as [`JpegError::UnsupportedFormat`] (kIOReturnError).
    fn of(request: &JpegDecode<'_>) -> Result<Self, JpegError> {
        let Some(layout) = request.format.chroma_divisors() else {
            return Ok(if request.scale == 1 {
                Self::Bgr
            } else {
                Self::Bgra
            });
        };
        let Some(sampled) = request.subsampling.chroma_divisors() else {
            return Ok(Self::Planes {
                layout: JpegSubsampling::Grey,
            });
        };
        let layout = match layout {
            (2, 2) => JpegSubsampling::Yuv420,
            (2, 1) => JpegSubsampling::Yuv422,
            // Measured: `444f` from 4:2:0 and 4:2:2 is kIOReturnError on
            // Apple's host.
            (1, 1) if sampled == (1, 1) => JpegSubsampling::Yuv444,
            _ => {
                return Err(JpegError::UnsupportedFormat(format!(
                    "{:?} from a {:?} stream, which Apple's hardware refuses too",
                    request.format, request.subsampling
                )));
            }
        };
        Ok(Self::Planes { layout })
    }
}

/// A rectangle of one plane, its x and width in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ByteRect {
    pub(super) x: usize,
    pub(super) y: usize,
    pub(super) width: usize,
    pub(super) height: usize,
}

/// One plane nvJPEG writes, the part of it copied back, and how an output
/// plane is made of that part.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct PlanePlan {
    /// What nvJPEG writes: bytes a row, and rows.
    pub(super) decoded: (usize, usize),
    /// The rows' pitch in device memory.
    pub(super) pitch: usize,
    /// Where the plane starts in device memory.
    pub(super) device_offset: usize,
    /// The part copied back.
    pub(super) copied: ByteRect,
    /// Where that part starts in page-locked memory, its rows packed.
    pub(super) staged_offset: usize,
    /// The copied part's axes, across and down, where the output plane is
    /// resampled from it; none where it is the output plane.
    pub(super) resample: Option<[Axis; 2]>,
}

/// What one decode asks of nvJPEG, and how its output is written.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Plan {
    pub(super) output: nvjpeg::nvjpegOutputFormat_t,
    /// nvJPEG's scale factor: 0 to 3 for 1 to 1/8.
    pub(super) scale_factor: nvjpeg::nvjpegScaleFactor_t,
    /// The region of interest, x, y, width and height, at scale 1.
    pub(super) roi: Option<[c_int; 4]>,
    pub(super) planes: Vec<PlanePlan>,
    pub(super) route: Route,
    pub(super) subsampling: JpegSubsampling,
    /// The output's size.
    pub(super) output_size: (usize, usize),
    /// The device memory the planes take.
    pub(super) device_bytes: usize,
    /// The page-locked memory the copies take.
    pub(super) staged_bytes: usize,
}

/// One output plane along one axis: the window of the image's plane, the
/// plane's length, where nvJPEG's samples start in it, how many it writes,
/// and the output's.
type Side = ((f64, f64), f64, usize, usize, usize);

/// One plane nvJPEG writes: its size, the part copied back, and that
/// part's axes where the output plane is resampled from it.
type Shape = ((usize, usize), ByteRect, Option<[Axis; 2]>);

impl Plan {
    /// Checks `image` against the request, before any work on the GPU, and
    /// says what nvJPEG is asked for and how its output is written.
    pub(super) fn of(image: &Image<'_>, request: &JpegDecode<'_>) -> Result<Self, JpegError> {
        let frame = &image.frame;
        let (width, height) = (u32::from(frame.width), u32::from(frame.height));
        if (width, height) != (request.width, request.height) {
            return Err(JpegError::SizeMismatch { width, height });
        }
        if frame.subsampling().and_then(subsampling) != Some(request.subsampling) {
            return Err(JpegError::SubsamplingMismatch);
        }
        if frame.components.len() == 3 && image.adobe_transform == Some(0) {
            return Err(JpegError::UnsupportedStream(
                "RGB components (Adobe transform 0)".into(),
            ));
        }
        if request.subsampling == JpegSubsampling::Yuv411 {
            // Measured: NVJPEG_STATUS_JPEG_NOT_SUPPORTED.
            return Err(JpegError::UnsupportedStream(
                "4:1:1, which nvJPEG's hardware decoder does not decode".into(),
            ));
        }
        let route = Route::of(request)?;
        let scale = request.scale;
        if !matches!(scale, 1 | 2 | 4 | 8) {
            return Err(JpegError::UnsupportedScale(scale));
        }
        // nvJPEG's scale: the request's, or finer where the layout's chroma
        // is finer than the stream's along an axis, so that no output plane
        // is made of samples coarser than the image's it covers. Only
        // 4:2:0's chroma into a 4:2:2 layout is: at 1/2 the reference takes
        // its rows from every chroma row of the image, and nvJPEG's at 1/2
        // has half of them (Cr 31.6 dB against 57 to 67 for every other
        // scaled plane of the 640x480 cases).
        let finest =
            |(across, down): (u32, u32)| match (route, request.subsampling.chroma_divisors()) {
                (Route::Planes { .. }, Some((sx, sy))) => {
                    (scale * across / sx).min(scale * down / sy).clamp(1, scale)
                }
                _ => scale,
            };
        let decode_scale = match route {
            Route::Planes { layout } => layout.chroma_divisors().map_or(scale, finest),
            _ => scale,
        };
        let scale_factor = match decode_scale {
            1 => 0,
            2 => 1,
            4 => 2,
            _ => 3,
        };
        let size = crate::usize_of;
        let region = request.region;
        let (output_width, output_height) = request.output_size();
        let (output_width, output_height) = (size(output_width), size(output_height));
        let whole = region
            == PixelRect {
                x: 0,
                y: 0,
                width,
                height,
            };
        // The region starts on an MCU, a multiple of 8, which a scale of 8
        // at most divides.
        if !region.x.is_multiple_of(decode_scale) || !region.y.is_multiple_of(decode_scale) {
            return Err(JpegError::Malfunction(format!(
                "a region at ({}, {}) off the 1/{decode_scale} grid",
                region.x, region.y
            )));
        }
        // What nvJPEG decodes, and where it starts in the image: the region
        // of interest at scale 1, else the whole image at the scale, from
        // which the region is cut; nvJPEG failed a region of interest with a
        // scale (`NVJPEG_STATUS_EXECUTION_FAILED` or `NVJPEG_STATUS_BAD_JPEG`).
        let (decoded, origin, roi) = if decode_scale == 1 && !whole {
            let roi = [region.x, region.y, region.width, region.height]
                .map(|value| c_int::try_from(value).unwrap_or(c_int::MAX));
            (
                (size(region.width), size(region.height)),
                (size(region.x), size(region.y)),
                Some(roi),
            )
        } else {
            (
                (
                    size(width.div_ceil(decode_scale)),
                    size(height.div_ceil(decode_scale)),
                ),
                (0, 0),
                None,
            )
        };
        let past = || JpegError::Malfunction("the region is past the decode".into());
        let mut shapes: Vec<Shape> = Vec::new();
        let output = if route == Route::Bgr {
            // At scale 1, what nvJPEG decodes is the output.
            let bytes = |value: usize| value.saturating_mul(3);
            if (output_width, output_height) != decoded {
                return Err(past());
            }
            shapes.push((
                (bytes(decoded.0), decoded.1),
                ByteRect {
                    x: 0,
                    y: 0,
                    width: bytes(output_width),
                    height: output_height,
                },
                None,
            ));
            nvjpeg::NVJPEG_OUTPUT_BGRI
        } else {
            let f = f64::from;
            let unit = f(decode_scale);
            let mut sides: Vec<[Side; 2]> = vec![[
                (
                    (f(region.x), f(region.width)),
                    f(width),
                    origin.0,
                    decoded.0,
                    output_width,
                ),
                (
                    (f(region.y), f(region.height)),
                    f(height),
                    origin.1,
                    decoded.1,
                    output_height,
                ),
            ]];
            // The chroma, its window the region over the stream's divisors
            // (the region starts on an MCU, so on a whole chroma sample), at
            // the layout's sampling, or the stream's into BGRA.
            let divisors = match route {
                Route::Planes { layout } => layout.chroma_divisors(),
                _ => request.subsampling.chroma_divisors(),
            };
            if let (Some((sx, sy)), Some((dx, dy))) =
                (request.subsampling.chroma_divisors(), divisors)
            {
                let (sx_size, sy_size) = (size(sx), size(sy));
                let chroma = [
                    (
                        (f(region.x) / f(sx), f(region.width) / f(sx)),
                        f(width.div_ceil(sx)),
                        origin.0 / sx_size,
                        decoded.0.div_ceil(sx_size),
                        output_width.div_ceil(size(dx)),
                    ),
                    (
                        (f(region.y) / f(sy), f(region.height) / f(sy)),
                        f(height.div_ceil(sy)),
                        origin.1 / sy_size,
                        decoded.1.div_ceil(sy_size),
                        output_height.div_ceil(size(dy)),
                    ),
                ];
                sides.push(chroma);
                sides.push(chroma);
            }
            // At scale 1 into a layout of the stream's sampling, the output
            // is the image's own samples, copied ([`convert`]).
            let samples = scale == 1
                && route
                    == Route::Planes {
                        layout: request.subsampling,
                    };
            for [across, down] in sides {
                let axis = |(window, plane, origin, decoded, output): Side| {
                    convert::axis(window, plane, (origin, unit), (decoded, output), samples)
                        .ok_or_else(past)
                };
                let ((columns, across_axis), (rows, down_axis)) = (axis(across)?, axis(down)?);
                let copied = ByteRect {
                    x: columns.start,
                    y: rows.start,
                    width: columns.len(),
                    height: rows.len(),
                };
                let resample = (!samples && !(across_axis.is_copy() && down_axis.is_copy()))
                    .then_some([across_axis, down_axis]);
                shapes.push(((across.3, down.3), copied, resample));
            }
            nvjpeg::NVJPEG_OUTPUT_YUV
        };
        let mut planes = Vec::with_capacity(shapes.len());
        let (mut device_bytes, mut staged_bytes) = (0usize, 0usize);
        let too_large = || JpegError::UnsupportedStream("an output too large to hold".into());
        for (decoded, copied, resample) in shapes {
            let pitch = decoded
                .0
                .max(1)
                .checked_next_multiple_of(PITCH_ALIGNMENT)
                .ok_or_else(too_large)?;
            planes.push(PlanePlan {
                decoded,
                pitch,
                device_offset: device_bytes,
                copied,
                staged_offset: staged_bytes,
                resample,
            });
            device_bytes = pitch
                .checked_mul(decoded.1)
                .and_then(|bytes| bytes.checked_add(device_bytes))
                .ok_or_else(too_large)?;
            staged_bytes = copied
                .width
                .checked_mul(copied.height)
                .and_then(|bytes| bytes.checked_add(staged_bytes))
                .ok_or_else(too_large)?;
        }
        Ok(Self {
            output,
            scale_factor,
            roi,
            planes,
            route,
            subsampling: request.subsampling,
            output_size: (output_width, output_height),
            device_bytes,
            staged_bytes,
        })
    }

    /// The scale nvJPEG decodes at: the request's, or finer (`Plan::of`).
    pub(super) fn decode_scale(&self) -> u32 {
        1u32 << self.scale_factor
    }

    /// Whether any output plane is resampled on the CPU.
    pub(super) fn resampled(&self) -> bool {
        self.planes.iter().any(|plane| plane.resample.is_some())
    }

    /// Writes the output, copied back into `staged` as [`Plan::planes`]
    /// lays it, into `planes` in `request`'s layout.
    pub(super) fn write(
        &self,
        staged: &[u8],
        request: &JpegDecode<'_>,
        planes: &mut [PlaneDestination<'_>],
    ) -> Result<(), JpegError> {
        let (width, height) = self.output_size;
        let ended = || JpegError::Malfunction("the staged output ends early".into());
        let copied = |plan: &PlanePlan| -> Result<&[u8], JpegError> {
            let len = plan
                .copied
                .width
                .checked_mul(plan.copied.height)
                .ok_or_else(ended)?;
            let end = plan.staged_offset.checked_add(len).ok_or_else(ended)?;
            staged.get(plan.staged_offset..end).ok_or_else(ended)
        };
        if self.route == Route::Bgr {
            let ([plan], [destination]) = (&self.planes[..], planes) else {
                return Err(JpegError::Malfunction("not one plane".into()));
            };
            return convert::write_bgra(copied(plan)?, (width, height), destination);
        }
        // Each output plane, its rows packed: the copied part, or what was
        // resampled of it.
        let made = self
            .planes
            .iter()
            .map(|plan| {
                let bytes = copied(plan)?;
                Ok(match &plan.resample {
                    None => Cow::Borrowed(bytes),
                    Some([across, down]) => {
                        let plane = convert::Plane {
                            bytes,
                            width: plan.copied.width,
                            height: plan.copied.height,
                        };
                        Cow::Owned(convert::resample(plane, across, down)?)
                    }
                })
            })
            .collect::<Result<Vec<_>, JpegError>>()?;
        let made: Vec<&[u8]> = made.iter().map(AsRef::as_ref).collect();
        let (luma, chroma) = match made[..] {
            [luma] => (luma, None),
            [luma, cb, cr] => (luma, Some((cb, cr))),
            _ => return Err(JpegError::Malfunction("neither one plane nor three".into())),
        };
        let layout = match self.route {
            Route::Planes { layout } => layout,
            Route::Bgra => {
                let [destination] = planes else {
                    return Err(JpegError::Malfunction("not one plane".into()));
                };
                let chroma = match (chroma, self.subsampling.chroma_divisors()) {
                    (Some((cb, cr)), Some((across, down))) => Some(convert::Chroma {
                        cb,
                        cr,
                        divisors: (crate::usize_of(across), crate::usize_of(down)),
                    }),
                    (None, None) => None,
                    _ => return Err(ended()),
                };
                return convert::write_ycbcr_bgra(luma, chroma, (width, height), destination);
            }
            Route::Bgr => return Err(ended()),
        };
        fn view(bytes: &[u8], pitch: usize) -> SampleView<'_> {
            SampleView {
                bytes,
                offset: 0,
                pitch,
                step: 1,
            }
        }
        let chroma = match (chroma, layout.chroma_divisors()) {
            (Some((cb, cr)), Some((across, _))) => {
                let pitch = width.div_ceil(crate::usize_of(across));
                Some([view(cb, pitch), view(cr, pitch)])
            }
            (None, None) => None,
            _ => return Err(ended()),
        };
        // The output, whole, at scale 1: what was made is it.
        let whole = JpegDecode {
            subsampling: layout,
            region: PixelRect {
                x: 0,
                y: 0,
                width: u32::try_from(width).unwrap_or(u32::MAX),
                height: u32::try_from(height).unwrap_or(u32::MAX),
            },
            scale: 1,
            ..*request
        };
        write_samples(
            &DecodedSamples {
                subsampling: layout,
                luma: view(luma, width),
                chroma,
            },
            &whole,
            planes,
        )
    }
}
