mod interlace_info;
mod stream;
pub(crate) mod transform;
mod zlib;

pub use self::stream::{DecodeOptions, Decoded, DecodingError, StreamingDecoder};
use self::stream::{FormatErrorInner, CHUNK_BUFFER_SIZE};
use self::transform::{create_transform_fn, TransformFn};

use std::io::{BufRead, BufReader, ErrorKind, Read};
use std::mem;

use crate::adam7::{self, Adam7Info};
use crate::chunk;
use crate::common::{
    BitDepth, BytesPerPixel, ColorType, Info, ParameterErrorKind, Transformations,
};
use crate::filter::{unfilter, FilterType};
use crate::FrameControl;

pub use interlace_info::InterlaceInfo;
use interlace_info::InterlaceInfoIter;

/*
pub enum InterlaceHandling {
    /// Outputs the raw rows
    RawRows,
    /// Fill missing the pixels from the existing ones
    Rectangle,
    /// Only fill the needed pixels
    Sparkle
}
*/

/// Output info.
///
/// This describes one particular frame of the image that was written into the output buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct OutputInfo {
    /// The pixel width of this frame.
    pub width: u32,
    /// The pixel height of this frame.
    pub height: u32,
    /// The chosen output color type.
    pub color_type: ColorType,
    /// The chosen output bit depth.
    pub bit_depth: BitDepth,
    /// The byte count of each scan line in the image.
    pub line_size: usize,
}

impl OutputInfo {
    /// Returns the size needed to hold a decoded frame
    /// If the output buffer was larger then bytes after this count should be ignored. They may
    /// still have been changed.
    pub fn buffer_size(&self) -> usize {
        self.line_size * self.height as usize
    }
}

#[derive(Clone, Copy, Debug)]
/// Limits on the resources the `Decoder` is allowed too use
pub struct Limits {
    /// maximum number of bytes the decoder is allowed to allocate, default is 64Mib
    pub bytes: usize,
}

impl Limits {
    pub(crate) fn reserve_bytes(&mut self, bytes: usize) -> Result<(), DecodingError> {
        if self.bytes >= bytes {
            self.bytes -= bytes;
            Ok(())
        } else {
            Err(DecodingError::LimitsExceeded)
        }
    }
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            bytes: 1024 * 1024 * 64,
        }
    }
}

/// PNG Decoder
pub struct Decoder<R: Read> {
    read_decoder: ReadDecoder<R>,
    /// Output transformations
    transform: Transformations,
}

/// A row of data with interlace information attached.
#[derive(Clone, Copy, Debug)]
pub struct InterlacedRow<'data> {
    data: &'data [u8],
    interlace: InterlaceInfo,
}

impl<'data> InterlacedRow<'data> {
    pub fn data(&self) -> &'data [u8] {
        self.data
    }

    pub fn interlace(&self) -> &InterlaceInfo {
        &self.interlace
    }
}

/// A row of data without interlace information.
#[derive(Clone, Copy, Debug)]
pub struct Row<'data> {
    data: &'data [u8],
}

impl<'data> Row<'data> {
    pub fn data(&self) -> &'data [u8] {
        self.data
    }
}

impl<R: Read> Decoder<R> {
    /// Create a new decoder configuration with default limits.
    pub fn new(r: R) -> Decoder<R> {
        Decoder::new_with_limits(r, Limits::default())
    }

    /// Create a new decoder configuration with custom limits.
    pub fn new_with_limits(r: R, limits: Limits) -> Decoder<R> {
        let mut decoder = StreamingDecoder::new();
        decoder.limits = limits;

        Decoder {
            read_decoder: ReadDecoder {
                reader: BufReader::with_capacity(CHUNK_BUFFER_SIZE, r),
                decoder,
            },
            transform: Transformations::IDENTITY,
        }
    }

    /// Create a new decoder configuration with custom `DecodeOptions`.
    pub fn new_with_options(r: R, decode_options: DecodeOptions) -> Decoder<R> {
        let mut decoder = StreamingDecoder::new_with_options(decode_options);
        decoder.limits = Limits::default();

        Decoder {
            read_decoder: ReadDecoder {
                reader: BufReader::with_capacity(CHUNK_BUFFER_SIZE, r),
                decoder,
            },
            transform: Transformations::IDENTITY,
        }
    }

    /// Limit resource usage.
    ///
    /// Note that your allocations, e.g. when reading into a pre-allocated buffer, are __NOT__
    /// considered part of the limits. Nevertheless, required intermediate buffers such as for
    /// singular lines is checked against the limit.
    ///
    /// Note that this is a best-effort basis.
    ///
    /// ```
    /// use std::fs::File;
    /// use png::{Decoder, Limits};
    /// // This image is 32×32, 1bit per pixel. The reader buffers one row which requires 4 bytes.
    /// let mut limits = Limits::default();
    /// limits.bytes = 3;
    /// let mut decoder = Decoder::new_with_limits(File::open("tests/pngsuite/basi0g01.png").unwrap(), limits);
    /// assert!(decoder.read_info().is_err());
    ///
    /// // This image is 32x32 pixels, so the decoder will allocate less than 10Kib
    /// let mut limits = Limits::default();
    /// limits.bytes = 10*1024;
    /// let mut decoder = Decoder::new_with_limits(File::open("tests/pngsuite/basi0g01.png").unwrap(), limits);
    /// assert!(decoder.read_info().is_ok());
    /// ```
    pub fn set_limits(&mut self, limits: Limits) {
        self.read_decoder.decoder.limits = limits;
    }

    /// Read the PNG header and return the information contained within.
    ///
    /// Most image metadata will not be read until `read_info` is called, so those fields will be
    /// None or empty.
    pub fn read_header_info(&mut self) -> Result<&Info<'static>, DecodingError> {
        self.read_decoder.read_header_info()
    }

    /// Reads all meta data until the first IDAT chunk
    pub fn read_info(mut self) -> Result<Reader<R>, DecodingError> {
        self.read_header_info()?;

        let mut reader = Reader {
            decoder: self.read_decoder,
            bpp: BytesPerPixel::One,
            subframe: SubframeInfo::not_yet_init(),
            remaining_frames: 0, // Temporary value - fixed below after reading `acTL` and `fcTL`.
            data_stream: Vec::new(),
            prev_start: 0,
            current_start: 0,
            transform: self.transform,
            transform_fn: None,
            scratch_buffer: Vec::new(),
            finished: false,
        };

        // Check if the decoding buffer of a single raw line has a valid size.
        if reader.info().checked_raw_row_length().is_none() {
            return Err(DecodingError::LimitsExceeded);
        }

        // Check if the output buffer has a valid size.
        let (width, height) = reader.info().size();
        let (color, depth) = reader.output_color_type();
        let rowlen = color
            .checked_raw_row_length(depth, width)
            .ok_or(DecodingError::LimitsExceeded)?
            - 1;
        let height: usize =
            std::convert::TryFrom::try_from(height).map_err(|_| DecodingError::LimitsExceeded)?;
        if rowlen.checked_mul(height).is_none() {
            return Err(DecodingError::LimitsExceeded);
        }

        reader.read_until_image_data()?;

        reader.remaining_frames = match reader.info().animation_control.as_ref() {
            None => 1, // No `acTL` => only expecting `IDAT` frame.
            Some(animation) => {
                let mut num_frames = animation.num_frames as usize;
                if reader.info().frame_control.is_none() {
                    // No `fcTL` before `IDAT` => `IDAT` is not part of the animation, but
                    // represents an *extra*, default frame for non-APNG-aware decoders.
                    num_frames += 1;
                }
                num_frames
            }
        };
        Ok(reader)
    }

    /// Set the allowed and performed transformations.
    ///
    /// A transformation is a pre-processing on the raw image data modifying content or encoding.
    /// Many options have an impact on memory or CPU usage during decoding.
    pub fn set_transformations(&mut self, transform: Transformations) {
        self.transform = transform;
    }

    /// Set the decoder to ignore all text chunks while parsing.
    ///
    /// eg.
    /// ```
    /// use std::fs::File;
    /// use png::Decoder;
    /// let mut decoder = Decoder::new(File::open("tests/pngsuite/basi0g01.png").unwrap());
    /// decoder.set_ignore_text_chunk(true);
    /// assert!(decoder.read_info().is_ok());
    /// ```
    pub fn set_ignore_text_chunk(&mut self, ignore_text_chunk: bool) {
        self.read_decoder
            .decoder
            .set_ignore_text_chunk(ignore_text_chunk);
    }

    /// Set the decoder to ignore iccp chunks while parsing.
    ///
    /// eg.
    /// ```
    /// use std::fs::File;
    /// use png::Decoder;
    /// let mut decoder = Decoder::new(File::open("tests/iccp/broken_iccp.png").unwrap());
    /// decoder.set_ignore_iccp_chunk(true);
    /// assert!(decoder.read_info().is_ok());
    /// ```
    pub fn set_ignore_iccp_chunk(&mut self, ignore_iccp_chunk: bool) {
        self.read_decoder
            .decoder
            .set_ignore_iccp_chunk(ignore_iccp_chunk);
    }

    /// Set the decoder to ignore and not verify the Adler-32 checksum
    /// and CRC code.
    pub fn ignore_checksums(&mut self, ignore_checksums: bool) {
        self.read_decoder
            .decoder
            .set_ignore_adler32(ignore_checksums);
        self.read_decoder.decoder.set_ignore_crc(ignore_checksums);
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ImageDataCompletionStatus {
    ExpectingMoreData,
    Done,
}

struct ReadDecoder<R: Read> {
    reader: BufReader<R>,
    decoder: StreamingDecoder,
}

impl<R: Read> ReadDecoder<R> {
    /// Returns the next decoded chunk. If the chunk is an ImageData chunk, its contents are written
    /// into image_data.
    fn decode_next(&mut self, image_data: &mut Vec<u8>) -> Result<Decoded, DecodingError> {
        loop {
            let (consumed, result) = {
                let buf = self.reader.fill_buf()?;
                if buf.is_empty() {
                    return Err(DecodingError::IoError(ErrorKind::UnexpectedEof.into()));
                }
                self.decoder.update(buf, image_data)?
            };
            self.reader.consume(consumed);
            match result {
                Decoded::Nothing => (),
                result => return Ok(result),
            }
        }
    }

    pub fn read_header_info(&mut self) -> Result<&Info<'static>, DecodingError> {
        while self.info().is_none() {
            let mut buf = Vec::new();
            if let Decoded::ImageEnd = self.decode_next(&mut buf)? {
                unreachable!()
            }
            assert!(buf.is_empty());
        }
        Ok(self.info().unwrap())
    }

    pub fn read_until_image_data(&mut self) -> Result<(), DecodingError> {
        loop {
            // This is somewhat ugly. The API requires us to pass a buffer to decode_next but we
            // know that we will stop before reading any image data from the stream. Thus pass an
            // empty buffer and assert that remains empty.
            let mut buf = Vec::new();
            let state = self.decode_next(&mut buf)?;
            assert!(buf.is_empty());

            match state {
                Decoded::ChunkBegin(_, chunk::IDAT) | Decoded::ChunkBegin(_, chunk::fdAT) => break,
                Decoded::ImageEnd => {
                    return Err(DecodingError::Format(
                        FormatErrorInner::MissingImageData.into(),
                    ))
                }
                // Ignore all other chunk events. Any other chunk may be between IDAT chunks, fdAT
                // chunks and their control chunks.
                _ => {}
            }
        }
        Ok(())
    }

    pub fn decode_image_data(
        &mut self,
        image_data: &mut Vec<u8>,
    ) -> Result<ImageDataCompletionStatus, DecodingError> {
        match self.decode_next(image_data)? {
            Decoded::ImageData => Ok(ImageDataCompletionStatus::ExpectingMoreData),
            Decoded::ImageDataFlushed => Ok(ImageDataCompletionStatus::Done),
            // Similarily to `finish_decoding` we ignore other events that may happen
            // within an `IDAT` / `fdAT` chunks sequence.
            Decoded::Nothing
            | Decoded::ChunkComplete(_, _)
            | Decoded::ChunkBegin(_, _)
            | Decoded::PartialChunk(_) => Ok(ImageDataCompletionStatus::ExpectingMoreData),
            // Similarily to `finish_decoding` other events should not happen
            // within an `IDAT` / `fdAT` chunks sequence.
            unexpected => unreachable!("{:?}", unexpected),
        }
    }

    /// Consumes and discards the rest of an `IDAT` / `fdAT` chunk sequence.
    pub fn finish_decoding(&mut self) -> Result<(), DecodingError> {
        loop {
            let mut to_be_discarded = vec![];
            if let ImageDataCompletionStatus::Done = self.decode_image_data(&mut to_be_discarded)? {
                return Ok(());
            }
        }
    }

    pub fn read_until_end_of_input(&mut self) -> Result<(), DecodingError> {
        while !matches!(self.decode_next(&mut vec![])?, Decoded::ImageEnd) {}
        Ok(())
    }

    pub fn info(&self) -> Option<&Info<'static>> {
        self.decoder.info.as_ref()
    }
}

/// PNG reader (mostly high-level interface)
///
/// Provides a high level that iterates over lines or whole images.
pub struct Reader<R: Read> {
    decoder: ReadDecoder<R>,
    bpp: BytesPerPixel,
    subframe: SubframeInfo,
    /// How many frames remain to be decoded.  Decremented after each `IDAT` or `fdAT` sequence.
    remaining_frames: usize,
    /// Vec containing the uncompressed image data currently being processed.
    data_stream: Vec<u8>,
    /// Index in `data_stream` where the previous row starts.
    prev_start: usize,
    /// Index in `data_stream` where the current row starts.
    current_start: usize,
    /// Output transformations
    transform: Transformations,
    /// Function that can transform decompressed, unfiltered rows into final output.
    /// See the `transform.rs` module for more details.
    transform_fn: Option<TransformFn>,
    /// This buffer is only used so that `next_row` and `next_interlaced_row` can return reference
    /// to a byte slice. In a future version of this library, this buffer will be removed and
    /// `next_row` and `next_interlaced_row` will write directly into a user provided output buffer.
    scratch_buffer: Vec<u8>,
    /// Whether `ImageEnd` was already reached by `fn finish`.
    finished: bool,
}

/// The subframe specific information.
///
/// In APNG the frames are constructed by combining previous frame and a new subframe (through a
/// combination of `dispose_op` and `overlay_op`). These sub frames specify individual dimension
/// information and reuse the global interlace options. This struct encapsulates the state of where
/// in a particular IDAT-frame or subframe we are.
struct SubframeInfo {
    width: u32,
    height: u32,
    rowlen: usize,
    current_interlace_info: Option<InterlaceInfo>,
    interlace_info_iter: InterlaceInfoIter,
    consumed_and_flushed: bool,
}

impl<R: Read> Reader<R> {
    /// Advances to the start of the next animation frame and
    /// returns a reference to the `FrameControl` info that describes it.
    /// Skips and discards the image data of the previous frame if necessary.
    ///
    /// Returns a [`ParameterError`] when there are no more animation frames.
    /// To avoid this the caller can check if [`Info::animation_control`] exists
    /// and consult [`AnimationControl::num_frames`].
    pub fn next_frame_info(&mut self) -> Result<&FrameControl, DecodingError> {
        let remaining_frames = if self.subframe.consumed_and_flushed {
            self.remaining_frames
        } else {
            // One remaining frame will be consumed by the `finish_decoding` call below.
            self.remaining_frames - 1
        };
        if remaining_frames == 0 {
            return Err(DecodingError::Parameter(
                ParameterErrorKind::PolledAfterEndOfImage.into(),
            ));
        }

        if !self.subframe.consumed_and_flushed {
            self.subframe.current_interlace_info = None;
            self.finish_decoding()?;
        }
        self.read_until_image_data()?;

        // The PNG standard (and `StreamingDecoder `) guarantes that there is an `fcTL` chunk
        // before the start of image data in a sequence of `fdAT` chunks.  Therefore `unwrap`
        // below is guaranteed to not panic.
        Ok(self.info().frame_control.as_ref().unwrap())
    }

    /// Reads all meta data until the next frame data starts.
    /// Requires IHDR before the IDAT and fcTL before fdAT.
    fn read_until_image_data(&mut self) -> Result<(), DecodingError> {
        self.decoder.read_until_image_data()?;

        self.subframe = SubframeInfo::new(self.info());
        self.bpp = self.info().bpp_in_prediction();
        self.data_stream.clear();
        self.current_start = 0;
        self.prev_start = 0;

        // Allocate output buffer.
        let buflen = self.output_line_size(self.subframe.width);
        self.decoder.decoder.limits.reserve_bytes(buflen)?;

        self.prev_start = self.current_start;

        Ok(())
    }

    /// Get information on the image.
    ///
    /// The structure will change as new frames of an animated image are decoded.
    pub fn info(&self) -> &Info<'static> {
        self.decoder.info().unwrap()
    }

    /// Decodes the next frame into `buf`.
    ///
    /// Note that this decodes raw subframes that need to be mixed according to blend-op and
    /// dispose-op by the caller.
    ///
    /// The caller must always provide a buffer large enough to hold a complete frame (the APNG
    /// specification restricts subframes to the dimensions given in the image header). The region
    /// that has been written be checked afterwards by calling `info` after a successful call and
    /// inspecting the `frame_control` data. This requirement may be lifted in a later version of
    /// `png`.
    ///
    /// Output lines will be written in row-major, packed matrix with width and height of the read
    /// frame (or subframe), all samples are in big endian byte order where this matters.
    pub fn next_frame(&mut self, buf: &mut [u8]) -> Result<OutputInfo, DecodingError> {
        if self.remaining_frames == 0 {
            return Err(DecodingError::Parameter(
                ParameterErrorKind::PolledAfterEndOfImage.into(),
            ));
        } else if self.subframe.consumed_and_flushed {
            // Advance until the next `fdAT`
            // (along the way we should encounter the fcTL for this frame).
            self.read_until_image_data()?;
        }

        if buf.len() < self.output_buffer_size() {
            return Err(DecodingError::Parameter(
                ParameterErrorKind::ImageBufferSize {
                    expected: buf.len(),
                    actual: self.output_buffer_size(),
                }
                .into(),
            ));
        }

        let (color_type, bit_depth) = self.output_color_type();
        let output_info = OutputInfo {
            width: self.subframe.width,
            height: self.subframe.height,
            color_type,
            bit_depth,
            line_size: self.output_line_size(self.subframe.width),
        };

        if self.info().interlaced {
            let stride = self.output_line_size(self.info().width);
            let samples = color_type.samples() as u8;
            let bits_pp = samples * (bit_depth as u8);
            while let Some(InterlacedRow {
                data: row,
                interlace,
                ..
            }) = self.next_interlaced_row()?
            {
                // `unwrap` won't panic, because we checked `self.info().interlaced` above.
                let adam7info = interlace.get_adam7_info().unwrap();
                adam7::expand_pass(buf, stride, row, adam7info, bits_pp);
            }
        } else {
            let current_interlace_info = self.subframe.current_interlace_info.as_ref();
            let already_done_rows = current_interlace_info
                .map(|info| info.line_number())
                .unwrap_or(self.subframe.height);

            for row in buf
                .chunks_exact_mut(output_info.line_size)
                .take(self.subframe.height as usize)
                .skip(already_done_rows as usize)
            {
                self.next_interlaced_row_impl(self.subframe.rowlen, row)?;
            }
        }

        // Advance over the rest of data for this (sub-)frame.
        self.finish_decoding()?;

        Ok(output_info)
    }

    fn mark_subframe_as_consumed_and_flushed(&mut self) {
        assert!(self.remaining_frames > 0);
        self.remaining_frames -= 1;

        self.subframe.consumed_and_flushed = true;
    }

    /// Advance over the rest of data for this (sub-)frame.
    /// Called after decoding the last row of a frame.
    fn finish_decoding(&mut self) -> Result<(), DecodingError> {
        // Double-check that all rows of this frame have been decoded (i.e. that the potential
        // `finish_decoding` call below won't be discarding any data).
        assert!(self.subframe.current_interlace_info.is_none());

        // Discard the remaining data in the current sequence of `IDAT` or `fdAT` chunks.
        if !self.subframe.consumed_and_flushed {
            self.decoder.finish_decoding()?;
            self.mark_subframe_as_consumed_and_flushed();
        }

        Ok(())
    }

    /// Returns the next processed row of the image
    pub fn next_row(&mut self) -> Result<Option<Row>, DecodingError> {
        self.next_interlaced_row()
            .map(|v| v.map(|v| Row { data: v.data }))
    }

    /// Returns the next processed row of the image
    pub fn next_interlaced_row(&mut self) -> Result<Option<InterlacedRow>, DecodingError> {
        let interlace = match self.subframe.current_interlace_info.as_ref() {
            None => {
                self.finish_decoding()?;
                return Ok(None);
            }
            Some(interlace) => *interlace,
        };
        if interlace.line_number() == 0 {
            self.prev_start = self.current_start;
        }
        let rowlen = match interlace {
            InterlaceInfo::Null(_) => self.subframe.rowlen,
            InterlaceInfo::Adam7(Adam7Info { width, .. }) => {
                self.info().raw_row_length_from_width(width)
            }
        };
        let width = match interlace {
            InterlaceInfo::Adam7(Adam7Info { width, .. }) => width,
            InterlaceInfo::Null(_) => self.subframe.width,
        };
        let output_line_size = self.output_line_size(width);

        // TODO: change the interface of `next_interlaced_row` to take an output buffer instead of
        // making us return a reference to a buffer that we own.
        let mut output_buffer = mem::take(&mut self.scratch_buffer);
        output_buffer.resize(output_line_size, 0u8);
        let ret = self.next_interlaced_row_impl(rowlen, &mut output_buffer);
        self.scratch_buffer = output_buffer;
        ret?;

        Ok(Some(InterlacedRow {
            data: &self.scratch_buffer[..output_line_size],
            interlace,
        }))
    }

    /// Read the rest of the image and chunks and finish up, including text chunks or others
    /// This will discard the rest of the image if the image is not read already with [`Reader::next_frame`], [`Reader::next_row`] or [`Reader::next_interlaced_row`]
    pub fn finish(&mut self) -> Result<(), DecodingError> {
        if self.finished {
            return Err(DecodingError::Parameter(
                ParameterErrorKind::PolledAfterEndOfImage.into(),
            ));
        }

        self.remaining_frames = 0;
        self.data_stream.clear();
        self.current_start = 0;
        self.prev_start = 0;
        self.decoder.read_until_end_of_input()?;

        self.finished = true;
        Ok(())
    }

    /// Fetch the next interlaced row and filter it according to our own transformations.
    fn next_interlaced_row_impl(
        &mut self,
        rowlen: usize,
        output_buffer: &mut [u8],
    ) -> Result<(), DecodingError> {
        self.next_raw_interlaced_row(rowlen)?;
        assert_eq!(self.current_start - self.prev_start, rowlen - 1);
        let row = &self.data_stream[self.prev_start..self.current_start];

        // Apply transformations and write resulting data to buffer.
        let transform_fn = {
            if self.transform_fn.is_none() {
                self.transform_fn = Some(create_transform_fn(self.info(), self.transform)?);
            }
            self.transform_fn.as_deref().unwrap()
        };
        transform_fn(row, output_buffer, self.info());

        self.subframe.current_interlace_info = self.subframe.interlace_info_iter.next();
        Ok(())
    }

    /// Returns the color type and the number of bits per sample
    /// of the data returned by `Reader::next_row` and Reader::frames`.
    pub fn output_color_type(&self) -> (ColorType, BitDepth) {
        use crate::common::ColorType::*;
        let t = self.transform;
        let info = self.info();
        if t == Transformations::IDENTITY {
            (info.color_type, info.bit_depth)
        } else {
            let bits = match info.bit_depth as u8 {
                16 if t.intersects(Transformations::STRIP_16) => 8,
                n if n < 8
                    && (t.contains(Transformations::EXPAND)
                        || t.contains(Transformations::ALPHA)) =>
                {
                    8
                }
                n => n,
            };
            let color_type =
                if t.contains(Transformations::EXPAND) || t.contains(Transformations::ALPHA) {
                    let has_trns = info.trns.is_some() || t.contains(Transformations::ALPHA);
                    match info.color_type {
                        Grayscale if has_trns => GrayscaleAlpha,
                        Rgb if has_trns => Rgba,
                        Indexed if has_trns => Rgba,
                        Indexed => Rgb,
                        ct => ct,
                    }
                } else {
                    info.color_type
                };
            (color_type, BitDepth::from_u8(bits).unwrap())
        }
    }

    /// Returns the number of bytes required to hold a deinterlaced image frame
    /// that is decoded using the given input transformations.
    pub fn output_buffer_size(&self) -> usize {
        let (width, height) = self.info().size();
        let size = self.output_line_size(width);
        size * height as usize
    }

    /// Returns the number of bytes required to hold a deinterlaced row.
    pub fn output_line_size(&self, width: u32) -> usize {
        let (color, depth) = self.output_color_type();
        color.raw_row_length_from_width(depth, width) - 1
    }

    /// Write the next raw interlaced row into `self.prev`.
    ///
    /// The scanline is filtered against the previous scanline according to the specification.
    fn next_raw_interlaced_row(&mut self, rowlen: usize) -> Result<(), DecodingError> {
        // Read image data until we have at least one full row (but possibly more than one).
        while self.data_stream.len() - self.current_start < rowlen {
            if self.subframe.consumed_and_flushed {
                return Err(DecodingError::Format(
                    FormatErrorInner::NoMoreImageData.into(),
                ));
            }

            // Clear the current buffer before appending more data.
            if self.prev_start > 0 {
                self.data_stream.copy_within(self.prev_start.., 0);
                self.data_stream
                    .truncate(self.data_stream.len() - self.prev_start);
                self.current_start -= self.prev_start;
                self.prev_start = 0;
            }

            match self.decoder.decode_image_data(&mut self.data_stream)? {
                ImageDataCompletionStatus::ExpectingMoreData => (),
                ImageDataCompletionStatus::Done => self.mark_subframe_as_consumed_and_flushed(),
            }
        }

        // Get a reference to the current row and point scan_start to the next one.
        let (prev, row) = self.data_stream.split_at_mut(self.current_start);

        // Unfilter the row.
        let filter = FilterType::from_u8(row[0]).ok_or(DecodingError::Format(
            FormatErrorInner::UnknownFilterMethod(row[0]).into(),
        ))?;
        unfilter(
            filter,
            self.bpp,
            &prev[self.prev_start..],
            &mut row[1..rowlen],
        );

        self.prev_start = self.current_start + 1;
        self.current_start += rowlen;

        Ok(())
    }
}

impl SubframeInfo {
    fn not_yet_init() -> Self {
        SubframeInfo {
            width: 0,
            height: 0,
            rowlen: 0,
            current_interlace_info: None,
            interlace_info_iter: InterlaceInfoIter::empty(),
            consumed_and_flushed: false,
        }
    }

    fn new(info: &Info) -> Self {
        // The apng fctnl overrides width and height.
        // All other data is set by the main info struct.
        let (width, height) = if let Some(fc) = info.frame_control {
            (fc.width, fc.height)
        } else {
            (info.width, info.height)
        };

        let mut interlace_info_iter = InterlaceInfoIter::new(width, height, info.interlaced);
        let current_interlace_info = interlace_info_iter.next();
        SubframeInfo {
            width,
            height,
            rowlen: info.raw_row_length_from_width(width),
            current_interlace_info,
            interlace_info_iter,
            consumed_and_flushed: false,
        }
    }
}
