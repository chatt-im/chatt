#![cfg_attr(not(opus_codec_rust_packet_ops), allow(dead_code))]

#[cfg(not(opus_codec_frame_bounded_extensions))]
use super::EXTENSION_MIN_DATA_ID;
#[cfg(opus_codec_frame_bounded_extensions)]
use super::{EXTENSION_FRAME_BOUNDED_MIN_DATA_ID, EXTENSION_REPEAT_ID};
use super::{
    EXTENSION_FRAME_SEPARATOR_ID, EXTENSION_LENGTH_CHUNK, EXTENSION_LENGTH_CHUNK_BYTE,
    EXTENSION_MAX_ID, EXTENSION_MULTI_FRAME_SEPARATOR, EXTENSION_ONE_FRAME_SEPARATOR,
    EXTENSION_PADDING_ID, EXTENSION_SHORT_ID_MAX, MAX_FRAMES_PER_PACKET, PacketExtension,
};
use crate::error::{Error, Result};

#[cfg(not(opus_codec_frame_bounded_extensions))]
fn skip_extension(data: &[u8], start: usize) -> Result<(usize, usize)> {
    let remaining = data.len().saturating_sub(start);
    if remaining == 0 {
        return Ok((start, 0));
    }

    let first = data[start];
    let id = first >> 1;
    let len_flag = first & 1;
    if id == EXTENSION_PADDING_ID && len_flag == 1 {
        return Ok((start + 1, 1));
    }
    if id > EXTENSION_PADDING_ID && id < EXTENSION_SHORT_ID_MAX {
        let consumed = 1 + usize::from(len_flag);
        if remaining < consumed {
            return Err(Error::InvalidPacket);
        }
        return Ok((start + consumed, 1));
    }
    if len_flag == 0 {
        return Ok((data.len(), 1));
    }

    let mut cursor = start + 1;
    let mut header_size = 1usize;
    let mut payload_len = 0usize;
    loop {
        if cursor >= data.len() {
            return Err(Error::InvalidPacket);
        }
        let byte = usize::from(data[cursor]);
        payload_len = payload_len.checked_add(byte).ok_or(Error::InternalError)?;
        header_size += 1;
        cursor += 1;
        if byte != EXTENSION_LENGTH_CHUNK {
            break;
        }
    }

    if payload_len > data.len().saturating_sub(cursor) {
        return Err(Error::InvalidPacket);
    }
    Ok((cursor + payload_len, header_size))
}

#[cfg(opus_codec_frame_bounded_extensions)]
fn skip_extension_payload_frame_bounded(
    data: &[u8],
    start: usize,
    id_byte: u8,
    trailing_short_len: usize,
) -> Result<(usize, usize)> {
    let id = id_byte >> 1;
    let len_flag = id_byte & 1;
    let mut cursor = start;
    let mut header_size = 0usize;

    if (id == EXTENSION_PADDING_ID && len_flag == 1) || id == EXTENSION_REPEAT_ID {
        return Ok((cursor, header_size));
    }
    if id > EXTENSION_PADDING_ID && id < EXTENSION_SHORT_ID_MAX {
        let consumed = usize::from(len_flag);
        if data.len().saturating_sub(cursor) < consumed {
            return Err(Error::InvalidPacket);
        }
        return Ok((cursor + consumed, header_size));
    }
    if len_flag == 0 {
        if data.len().saturating_sub(cursor) < trailing_short_len {
            return Err(Error::InvalidPacket);
        }
        return Ok((data.len() - trailing_short_len, header_size));
    }

    let mut payload_len = 0usize;
    loop {
        if cursor >= data.len() {
            return Err(Error::InvalidPacket);
        }
        let byte = usize::from(data[cursor]);
        payload_len = payload_len.checked_add(byte).ok_or(Error::InternalError)?;
        header_size += 1;
        cursor += 1;
        if byte != EXTENSION_LENGTH_CHUNK {
            break;
        }
    }

    if payload_len > data.len().saturating_sub(cursor) {
        return Err(Error::InvalidPacket);
    }
    Ok((cursor + payload_len, header_size))
}

#[cfg(opus_codec_frame_bounded_extensions)]
fn skip_extension_frame_bounded(data: &[u8], start: usize) -> Result<(usize, usize)> {
    let id_byte = *data.get(start).ok_or(Error::InvalidPacket)?;
    let (next, payload_header_size) =
        skip_extension_payload_frame_bounded(data, start + 1, id_byte, 0)?;
    Ok((next, payload_header_size + 1))
}

#[cfg(not(opus_codec_frame_bounded_extensions))]
fn parse_extensions(data: &[u8], _frame_count: usize) -> Result<Vec<PacketExtension>> {
    let mut cursor = 0usize;
    let mut current_frame = 0usize;
    let mut extensions = Vec::new();

    while cursor < data.len() {
        let entry = data[cursor];
        let id = entry >> 1;
        let len_flag = entry & 1;
        let (next, header_size) = skip_extension(data, cursor)?;
        if id >= EXTENSION_MIN_DATA_ID {
            if current_frame >= MAX_FRAMES_PER_PACKET {
                return Err(Error::InvalidPacket);
            }
            let data_start = cursor
                .checked_add(header_size)
                .ok_or(Error::InternalError)?;
            extensions.push(PacketExtension {
                id,
                frame: u8::try_from(current_frame).map_err(|_| Error::InvalidPacket)?,
                data: data[data_start..next].to_vec(),
            });
        } else if id == EXTENSION_FRAME_SEPARATOR_ID {
            if len_flag == 0 {
                current_frame += 1;
            } else if cursor + 1 < data.len() {
                current_frame += usize::from(data[cursor + 1]);
            }
            if current_frame >= MAX_FRAMES_PER_PACKET {
                return Err(Error::InvalidPacket);
            }
        }
        cursor = next;
    }

    Ok(extensions)
}

#[cfg(opus_codec_frame_bounded_extensions)]
#[derive(Debug)]
struct ExtensionIteratorFrameBounded<'a> {
    data: &'a [u8],
    nb_frames: usize,
    curr_pos: usize,
    repeat_data: usize,
    repeat_pos: usize,
    repeat_end: usize,
    repeat_frame: usize,
    curr_frame: usize,
    repeat_len_flag: u8,
    trailing_short_len: usize,
    last_long_end: Option<usize>,
}

#[cfg(opus_codec_frame_bounded_extensions)]
impl<'a> ExtensionIteratorFrameBounded<'a> {
    fn new(data: &'a [u8], nb_frames: usize) -> Self {
        Self {
            data,
            nb_frames,
            curr_pos: 0,
            repeat_data: 0,
            repeat_pos: 0,
            repeat_end: 0,
            repeat_frame: 0,
            curr_frame: 0,
            repeat_len_flag: 0,
            trailing_short_len: 0,
            last_long_end: None,
        }
    }

    fn next_extension(&mut self) -> Result<Option<PacketExtension>> {
        if self.nb_frames > MAX_FRAMES_PER_PACKET {
            return Err(Error::InvalidPacket);
        }
        loop {
            if let Some(extension) = self.next_repeated_extension()? {
                return Ok(Some(extension));
            }
            if self.curr_frame >= self.nb_frames {
                return Ok(None);
            }
            while self.curr_pos < self.data.len() {
                let start = self.curr_pos;
                let entry = self.data[start];
                let id = entry >> 1;
                let len_flag = entry & 1;
                let (next, header_size) = skip_extension_frame_bounded(self.data, start)?;
                self.curr_pos = next;

                if id == EXTENSION_FRAME_SEPARATOR_ID {
                    if len_flag == 0 {
                        self.curr_frame += 1;
                    } else if self.data[start + 1] != 0 {
                        self.curr_frame += usize::from(self.data[start + 1]);
                    } else {
                        continue;
                    }
                    if self.curr_frame >= self.nb_frames {
                        return Err(Error::InvalidPacket);
                    }
                    self.repeat_data = self.curr_pos;
                    self.last_long_end = None;
                    self.trailing_short_len = 0;
                } else if id == EXTENSION_REPEAT_ID {
                    self.repeat_len_flag = len_flag;
                    self.repeat_frame = self.curr_frame + 1;
                    self.repeat_end = start;
                    self.repeat_pos = self.repeat_data;
                    break;
                } else if id >= EXTENSION_FRAME_BOUNDED_MIN_DATA_ID {
                    if id >= EXTENSION_SHORT_ID_MAX {
                        self.last_long_end = Some(self.curr_pos);
                        self.trailing_short_len = 0;
                    } else {
                        self.trailing_short_len = self
                            .trailing_short_len
                            .checked_add(usize::from(len_flag))
                            .ok_or(Error::InternalError)?;
                    }
                    let data_start = start.checked_add(header_size).ok_or(Error::InternalError)?;
                    return Ok(Some(PacketExtension {
                        id,
                        frame: u8::try_from(self.curr_frame).map_err(|_| Error::InvalidPacket)?,
                        data: self.data[data_start..next].to_vec(),
                    }));
                }
            }
            if self.curr_pos >= self.data.len() {
                return Ok(None);
            }
        }
    }

    fn next_repeated_extension(&mut self) -> Result<Option<PacketExtension>> {
        while self.repeat_frame > 0 {
            while self.repeat_frame < self.nb_frames {
                while self.repeat_pos < self.repeat_end {
                    let original_id_byte = self.data[self.repeat_pos];
                    let (source_next, _) =
                        skip_extension_frame_bounded(self.data, self.repeat_pos)?;
                    self.repeat_pos = source_next;

                    if original_id_byte <= 3 {
                        continue;
                    }

                    let mut repeated_id_byte = original_id_byte;
                    if self.repeat_len_flag == 0
                        && self.repeat_frame + 1 >= self.nb_frames
                        && Some(self.repeat_pos) == self.last_long_end
                    {
                        repeated_id_byte &= !1;
                    }

                    let payload_start = self.curr_pos;
                    let (payload_next, header_size) = skip_extension_payload_frame_bounded(
                        self.data,
                        self.curr_pos,
                        repeated_id_byte,
                        self.trailing_short_len,
                    )?;
                    self.curr_pos = payload_next;
                    let data_start = payload_start
                        .checked_add(header_size)
                        .ok_or(Error::InternalError)?;
                    return Ok(Some(PacketExtension {
                        id: repeated_id_byte >> 1,
                        frame: u8::try_from(self.repeat_frame).map_err(|_| Error::InvalidPacket)?,
                        data: self.data[data_start..payload_next].to_vec(),
                    }));
                }
                self.repeat_pos = self.repeat_data;
                self.repeat_frame += 1;
            }

            self.repeat_data = self.curr_pos;
            self.last_long_end = None;
            if self.repeat_len_flag == 0 {
                self.curr_frame += 1;
                if self.curr_frame >= self.nb_frames {
                    self.curr_pos = self.data.len();
                }
            }
            self.repeat_frame = 0;
        }
        Ok(None)
    }
}

#[cfg(opus_codec_frame_bounded_extensions)]
fn parse_extensions(data: &[u8], frame_count: usize) -> Result<Vec<PacketExtension>> {
    let mut iterator = ExtensionIteratorFrameBounded::new(data, frame_count);
    let mut extensions = Vec::new();
    while let Some(extension) = iterator.next_extension()? {
        extensions.push(extension);
    }
    Ok(extensions)
}

fn ensure_extension_space(out: &[u8], len_limit: usize, additional: usize) -> Result<()> {
    let required = out
        .len()
        .checked_add(additional)
        .ok_or(Error::InternalError)?;
    if required > len_limit {
        return Err(Error::BufferTooSmall);
    }
    Ok(())
}

fn write_extension_frame_separator(
    out: &mut Vec<u8>,
    len_limit: usize,
    current_frame: &mut usize,
    frame: usize,
) -> Result<()> {
    if frame == *current_frame {
        return Ok(());
    }

    let diff = frame
        .checked_sub(*current_frame)
        .ok_or(Error::InvalidPacket)?;
    if diff == 1 {
        ensure_extension_space(out, len_limit, 1)?;
        out.push(EXTENSION_ONE_FRAME_SEPARATOR);
    } else {
        ensure_extension_space(out, len_limit, 2)?;
        out.push(EXTENSION_MULTI_FRAME_SEPARATOR);
        out.push(u8::try_from(diff).map_err(|_| Error::InvalidPacket)?);
    }
    *current_frame = frame;
    Ok(())
}

fn write_extension_payload(
    out: &mut Vec<u8>,
    len_limit: usize,
    extension: &PacketExtension,
    last: bool,
) -> Result<()> {
    if extension.id < EXTENSION_SHORT_ID_MAX {
        if extension.data.len() > 1 {
            return Err(Error::InvalidPacket);
        }
        ensure_extension_space(out, len_limit, extension.data.len())?;
        out.extend_from_slice(&extension.data);
    } else {
        let length_bytes = if last {
            0
        } else {
            1 + extension.data.len() / EXTENSION_LENGTH_CHUNK
        };
        ensure_extension_space(out, len_limit, length_bytes + extension.data.len())?;
        if !last {
            out.extend(std::iter::repeat_n(
                EXTENSION_LENGTH_CHUNK_BYTE,
                extension.data.len() / EXTENSION_LENGTH_CHUNK,
            ));
            out.push(
                u8::try_from(extension.data.len() % EXTENSION_LENGTH_CHUNK)
                    .map_err(|_| Error::InternalError)?,
            );
        }
        out.extend_from_slice(&extension.data);
    }
    Ok(())
}

fn write_extension_entry(
    out: &mut Vec<u8>,
    len_limit: usize,
    extension: &PacketExtension,
    last: bool,
) -> Result<()> {
    ensure_extension_space(out, len_limit, 1)?;
    let len_flag = if extension.id < EXTENSION_SHORT_ID_MAX {
        u8::try_from(extension.data.len()).map_err(|_| Error::InternalError)?
    } else {
        u8::from(!last)
    };
    out.push(extension.id << 1 | len_flag);
    write_extension_payload(out, len_limit, extension, last)
}

#[cfg(not(opus_codec_frame_bounded_extensions))]
pub(super) fn generate_extensions(
    extensions: &[PacketExtension],
    len_limit: usize,
    _frame_count: usize,
) -> Result<Vec<u8>> {
    if extensions.is_empty() {
        return Ok(Vec::new());
    }

    let mut max_frame = 0usize;
    for ext in extensions {
        let frame = usize::from(ext.frame);
        if ext.id < EXTENSION_MIN_DATA_ID
            || ext.id > EXTENSION_MAX_ID
            || frame >= MAX_FRAMES_PER_PACKET
        {
            return Err(Error::InvalidPacket);
        }
        max_frame = max_frame.max(frame);
    }

    let mut out = Vec::new();
    let mut current_frame = 0usize;
    let mut written = 0usize;

    for frame in 0..=max_frame {
        for ext in extensions
            .iter()
            .filter(|ext| usize::from(ext.frame) == frame)
        {
            write_extension_frame_separator(&mut out, len_limit, &mut current_frame, frame)?;
            write_extension_entry(&mut out, len_limit, ext, written + 1 == extensions.len())?;
            written += 1;
        }
    }

    Ok(out)
}

#[cfg(opus_codec_frame_bounded_extensions)]
#[derive(Clone, Copy, Debug, Default)]
struct FrameBoundedRepeatPlan {
    trigger_index: Option<usize>,
    repeat_count: usize,
    last_long_idx: Option<usize>,
}

#[cfg(opus_codec_frame_bounded_extensions)]
struct FrameBoundedExtensionWriter<'a> {
    extensions: &'a [PacketExtension],
    len_limit: usize,
    extension_count: usize,
    frame_count: usize,
    frame_min_idx: Vec<usize>,
    frame_max_idx: Vec<usize>,
    frame_repeat_idx: Vec<usize>,
    out: Vec<u8>,
    current_frame: usize,
    written: usize,
}

#[cfg(opus_codec_frame_bounded_extensions)]
impl<'a> FrameBoundedExtensionWriter<'a> {
    fn new(
        extensions: &'a [PacketExtension],
        len_limit: usize,
        frame_count: usize,
    ) -> Result<Self> {
        if frame_count > MAX_FRAMES_PER_PACKET {
            return Err(Error::InvalidPacket);
        }

        let extension_count = extensions.len();
        let mut frame_min_idx = vec![extension_count; frame_count];
        let mut frame_max_idx = vec![0usize; frame_count];
        for (index, extension) in extensions.iter().enumerate() {
            let frame = usize::from(extension.frame);
            if frame >= frame_count
                || extension.id < EXTENSION_FRAME_BOUNDED_MIN_DATA_ID
                || extension.id > EXTENSION_MAX_ID
            {
                return Err(Error::InvalidPacket);
            }
            frame_min_idx[frame] = frame_min_idx[frame].min(index);
            frame_max_idx[frame] = frame_max_idx[frame].max(index + 1);
        }

        let frame_repeat_idx = frame_min_idx.clone();
        Ok(Self {
            extensions,
            len_limit,
            extension_count,
            frame_count,
            frame_min_idx,
            frame_max_idx,
            frame_repeat_idx,
            out: Vec::new(),
            current_frame: 0,
            written: 0,
        })
    }

    fn into_bytes(mut self) -> Result<Vec<u8>> {
        for frame in 0..self.frame_count {
            self.write_frame(frame)?;
        }

        debug_assert_eq!(self.written, self.extension_count);
        Ok(self.out)
    }

    fn write_frame(&mut self, frame: usize) -> Result<()> {
        let repeat_plan = self.repeat_plan(frame);
        if self.frame_min_idx[frame] >= self.frame_max_idx[frame] {
            return Ok(());
        }

        for index in self.frame_min_idx[frame]..self.frame_max_idx[frame] {
            if usize::from(self.extensions[index].frame) != frame {
                continue;
            }

            self.write_extension(index)?;
            if repeat_plan.trigger_index == Some(index) {
                self.write_repeated_payloads(frame, repeat_plan)?;
            }
        }
        Ok(())
    }

    fn write_extension(&mut self, index: usize) -> Result<()> {
        write_extension_frame_separator(
            &mut self.out,
            self.len_limit,
            &mut self.current_frame,
            usize::from(self.extensions[index].frame),
        )?;
        write_extension_entry(
            &mut self.out,
            self.len_limit,
            &self.extensions[index],
            self.written + 1 == self.extension_count,
        )?;
        self.written += 1;
        Ok(())
    }

    fn repeat_plan(&mut self, frame: usize) -> FrameBoundedRepeatPlan {
        let mut plan = FrameBoundedRepeatPlan::default();
        if frame + 1 >= self.frame_count || self.frame_min_idx[frame] >= self.frame_max_idx[frame] {
            return plan;
        }

        for index in self.frame_min_idx[frame]..self.frame_max_idx[frame] {
            if usize::from(self.extensions[index].frame) != frame {
                continue;
            }
            if !self.extension_repeats_to_final_frame(frame, index) {
                break;
            }

            if self.extensions[index].id >= EXTENSION_SHORT_ID_MAX {
                plan.last_long_idx = Some(self.frame_repeat_idx[self.frame_count - 1]);
            }

            self.advance_future_repeat_indexes(frame);
            plan.repeat_count += 1;
            plan.trigger_index = Some(index);
        }
        plan
    }

    fn extension_repeats_to_final_frame(&self, frame: usize, index: usize) -> bool {
        let mut future_frame = frame + 1;
        while future_frame < self.frame_count {
            if self.frame_repeat_idx[future_frame] >= self.frame_max_idx[future_frame] {
                break;
            }
            let repeated = &self.extensions[self.frame_repeat_idx[future_frame]];
            if repeated.id != self.extensions[index].id {
                break;
            }
            if repeated.id < EXTENSION_SHORT_ID_MAX
                && repeated.data.len() != self.extensions[index].data.len()
            {
                break;
            }
            future_frame += 1;
        }
        future_frame == self.frame_count
    }

    fn advance_future_repeat_indexes(&mut self, frame: usize) {
        for future_frame in (frame + 1)..self.frame_count {
            let mut next = self.frame_repeat_idx[future_frame] + 1;
            while next < self.frame_max_idx[future_frame]
                && usize::from(self.extensions[next].frame) != future_frame
            {
                next += 1;
            }
            self.frame_repeat_idx[future_frame] = next;
        }
    }

    fn write_repeated_payloads(
        &mut self,
        frame: usize,
        plan: FrameBoundedRepeatPlan,
    ) -> Result<()> {
        let Some(trigger_index) = plan.trigger_index else {
            return Ok(());
        };
        let repeated_count = plan
            .repeat_count
            .checked_mul(self.frame_count - (frame + 1))
            .ok_or(Error::InternalError)?;
        let last = self
            .written
            .checked_add(repeated_count)
            .ok_or(Error::InternalError)?
            == self.extension_count
            || (plan.last_long_idx.is_none() && trigger_index + 1 >= self.frame_max_idx[frame]);
        ensure_extension_space(&self.out, self.len_limit, 1)?;
        self.out.push(EXTENSION_REPEAT_ID << 1 | u8::from(!last));

        for future_frame in (frame + 1)..self.frame_count {
            self.write_repeated_frame_payloads(future_frame, plan.last_long_idx, last)?;
        }
        if last {
            self.current_frame += 1;
        }
        Ok(())
    }

    fn write_repeated_frame_payloads(
        &mut self,
        future_frame: usize,
        last_long_idx: Option<usize>,
        last: bool,
    ) -> Result<()> {
        let mut next = self.frame_min_idx[future_frame];
        while next < self.frame_repeat_idx[future_frame] {
            if usize::from(self.extensions[next].frame) == future_frame {
                write_extension_payload(
                    &mut self.out,
                    self.len_limit,
                    &self.extensions[next],
                    last && Some(next) == last_long_idx,
                )?;
                self.written += 1;
            }
            next += 1;
        }
        self.frame_min_idx[future_frame] = next;
        Ok(())
    }
}

#[cfg(opus_codec_frame_bounded_extensions)]
pub(super) fn generate_extensions(
    extensions: &[PacketExtension],
    len_limit: usize,
    frame_count: usize,
) -> Result<Vec<u8>> {
    if extensions.is_empty() {
        return Ok(Vec::new());
    }
    FrameBoundedExtensionWriter::new(extensions, len_limit, frame_count)?.into_bytes()
}

pub(super) fn parse_existing_extensions(
    data: &[u8],
    frame_count: usize,
) -> Result<Vec<PacketExtension>> {
    // Upstream repacketizer code treats extension-parse failures inside an
    // otherwise successfully parsed packet as OPUS_INTERNAL_ERROR.
    parse_extensions(data, frame_count).map_err(|_| Error::InternalError)
}
