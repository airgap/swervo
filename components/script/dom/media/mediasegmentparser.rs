/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The time ranges held by an MSE `SourceBuffer`, read from the media segments appended to it.
//!
//! The playback pipeline only sees opaque bytes per SourceBuffer, and its buffering query can't
//! say which presentation times a given SourceBuffer holds. MSE players (YouTube's) decide what
//! to fetch next from `SourceBuffer.buffered` as soon as `updateend` fires, so the ranges are
//! derived synchronously from the container while appending: WebM cluster and block timecodes,
//! fragmented-MP4 `tfdt` decode times plus `trun` sample durations.

/// Ranges closer than this are one range: frame-duration estimates and audio/video frame
/// boundaries leave hairline gaps between consecutive segments.
const COALESCE_SECONDS: f64 = 0.1;

#[derive(Default)]
pub(crate) struct MediaSegmentParser {
    /// Bytes of an element or box header (or small payload) not yet complete.
    pending: Vec<u8>,
    /// Payload bytes still to discard before the next header (frame data, `mdat`).
    skip: u64,
    format: Option<Format>,
    /// Sorted, disjoint `[start, end)` presentation-time ranges in seconds.
    ranges: Vec<(f64, f64)>,
}

enum Format {
    WebM(WebMState),
    Mp4(Mp4State),
}

struct WebMState {
    timecode_scale_ns: u64,
    cluster_timecode: u64,
    default_duration_ns: Option<u64>,
    /// Start of the most recent block, used to estimate frame durations when the stream
    /// declares none.
    previous_block_ns: Option<u64>,
    estimated_frame_ns: u64,
}

impl Default for WebMState {
    fn default() -> Self {
        WebMState {
            // The Matroska default TimecodeScale: millisecond timecodes.
            timecode_scale_ns: 1_000_000,
            cluster_timecode: 0,
            default_duration_ns: None,
            previous_block_ns: None,
            // A conservative frame length until two blocks give a real one.
            estimated_frame_ns: 20_000_000,
        }
    }
}

struct Mp4State {
    timescale: u32,
    trex_default_duration: u32,
    tfhd_default_duration: Option<u32>,
    /// Decode time of the next sample in the current track fragment.
    next_decode_time: u64,
}

impl Default for Mp4State {
    fn default() -> Self {
        Mp4State {
            timescale: 1000,
            trex_default_duration: 0,
            tfhd_default_duration: None,
            next_decode_time: 0,
        }
    }
}

// EBML element ids, with their length-marker bits kept as Matroska writes them.
const EBML_SEGMENT: u64 = 0x1853_8067;
const EBML_CLUSTER: u64 = 0x1F43_B675;
const EBML_INFO: u64 = 0x1549_A966;
const EBML_TRACKS: u64 = 0x1654_AE6B;
const EBML_TRACK_ENTRY: u64 = 0xAE;
const EBML_BLOCK_GROUP: u64 = 0xA0;
const EBML_TIMECODE_SCALE: u64 = 0x2A_D7B1;
const EBML_CLUSTER_TIMECODE: u64 = 0xE7;
const EBML_DEFAULT_DURATION: u64 = 0x23_E383;
const EBML_SIMPLE_BLOCK: u64 = 0xA3;
const EBML_BLOCK: u64 = 0xA1;

impl MediaSegmentParser {
    pub(crate) fn ranges(&self) -> &[(f64, f64)] {
        &self.ranges
    }

    /// Parse appended bytes; `timestamp_offset` is the SourceBuffer's `timestampOffset`.
    pub(crate) fn append(&mut self, bytes: &[u8], timestamp_offset: f64) {
        let mut bytes = bytes;
        if self.skip > 0 {
            let skipped = self.skip.min(bytes.len() as u64);
            self.skip -= skipped;
            bytes = &bytes[skipped as usize..];
        }
        if bytes.is_empty() {
            return;
        }
        self.pending.extend_from_slice(bytes);
        if self.format.is_none() {
            self.format = detect_format(&self.pending);
        }
        let consumed = match self.format {
            Some(Format::WebM(_)) => self.parse_webm(timestamp_offset),
            Some(Format::Mp4(_)) => self.parse_mp4(timestamp_offset),
            // Too few bytes to tell yet.
            None => 0,
        };
        self.pending.drain(..consumed);
    }

    /// `abort()` and the like: drop any partially received element.
    pub(crate) fn reset_partial(&mut self) {
        self.pending.clear();
        self.skip = 0;
    }

    /// `remove(start, end)`.
    pub(crate) fn remove(&mut self, start: f64, end: f64) {
        let mut kept = Vec::with_capacity(self.ranges.len() + 1);
        for &(range_start, range_end) in &self.ranges {
            if range_end <= start || range_start >= end {
                kept.push((range_start, range_end));
                continue;
            }
            if range_start < start {
                kept.push((range_start, start));
            }
            if range_end > end {
                kept.push((end, range_end));
            }
        }
        self.ranges = kept;
    }

    fn add_range(&mut self, start: f64, end: f64) {
        let (mut start, mut end) = (start, end);
        let mut merged = Vec::with_capacity(self.ranges.len() + 1);
        for &(range_start, range_end) in &self.ranges {
            if range_end + COALESCE_SECONDS < start || end + COALESCE_SECONDS < range_start {
                merged.push((range_start, range_end));
            } else {
                start = start.min(range_start);
                end = end.max(range_end);
            }
        }
        merged.push((start, end));
        merged.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.ranges = merged;
    }

    /// Returns how many bytes of `pending` were consumed.
    fn parse_webm(&mut self, timestamp_offset: f64) -> usize {
        let mut position = 0;
        loop {
            let data = &self.pending[position..];
            let Some((id, id_length)) = read_ebml_id(data) else {
                return position;
            };
            let Some((size, size_length)) = read_ebml_size(&data[id_length..]) else {
                return position;
            };
            let header = id_length + size_length;
            match id {
                // Masters whose children matter: step inside instead of over them.
                EBML_SEGMENT | EBML_CLUSTER | EBML_INFO | EBML_TRACKS | EBML_TRACK_ENTRY |
                EBML_BLOCK_GROUP => position += header,
                EBML_TIMECODE_SCALE | EBML_CLUSTER_TIMECODE | EBML_DEFAULT_DURATION => {
                    let size = size.unwrap_or(0) as usize;
                    if data.len() < header + size {
                        return position;
                    }
                    let value = read_uint(&data[header..header + size]);
                    let Some(Format::WebM(ref mut state)) = self.format else {
                        unreachable!("parse_webm runs for WebM streams");
                    };
                    match id {
                        EBML_TIMECODE_SCALE => state.timecode_scale_ns = value.max(1),
                        EBML_CLUSTER_TIMECODE => state.cluster_timecode = value,
                        _ => state.default_duration_ns = Some(value),
                    }
                    position += header + size;
                },
                EBML_SIMPLE_BLOCK | EBML_BLOCK => {
                    let Some(size) = size else {
                        return position;
                    };
                    // Track number (a vint), then a signed 16-bit timecode relative to the
                    // cluster. The frame data after it is not needed.
                    let payload = &data[header..];
                    let Some((_, track_length)) = read_ebml_size(payload) else {
                        return position;
                    };
                    if payload.len() < track_length + 2 {
                        return position;
                    }
                    let relative =
                        i16::from_be_bytes([payload[track_length], payload[track_length + 1]]);
                    let available = (data.len() - header) as u64;
                    self.webm_block(relative, timestamp_offset);
                    if available >= size {
                        position += header + size as usize;
                    } else {
                        self.skip = size - available;
                        return self.pending.len();
                    }
                },
                _ => {
                    // Anything else (EBML header, SeekHead, Cues, Tags, Void, track details):
                    // not needed, skip it whole.
                    let Some(size) = size else {
                        // Only masters may have an unknown size; look inside.
                        position += header;
                        continue;
                    };
                    let available = (data.len() - header) as u64;
                    if available >= size {
                        position += header + size as usize;
                    } else {
                        self.skip = size - available;
                        return self.pending.len();
                    }
                },
            }
        }
    }

    fn webm_block(&mut self, relative_timecode: i16, timestamp_offset: f64) {
        let Some(Format::WebM(ref mut state)) = self.format else {
            unreachable!("webm_block runs for WebM streams");
        };
        let timecode = (state.cluster_timecode as i64 + relative_timecode as i64).max(0) as u64;
        let start_ns = timecode.saturating_mul(state.timecode_scale_ns);
        if let Some(previous) = state.previous_block_ns &&
            start_ns > previous
        {
            state.estimated_frame_ns = start_ns - previous;
        }
        state.previous_block_ns = Some(start_ns);
        let duration_ns = state
            .default_duration_ns
            .unwrap_or(state.estimated_frame_ns);
        let start = start_ns as f64 / 1e9 + timestamp_offset;
        let end = (start_ns + duration_ns) as f64 / 1e9 + timestamp_offset;
        self.add_range(start, end);
    }

    /// Returns how many bytes of `pending` were consumed.
    fn parse_mp4(&mut self, timestamp_offset: f64) -> usize {
        let mut position = 0;
        loop {
            let data = &self.pending[position..];
            if data.len() < 8 {
                return position;
            }
            let short_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as u64;
            let box_type = [data[4], data[5], data[6], data[7]];
            let (size, header) = match short_size {
                1 => {
                    if data.len() < 16 {
                        return position;
                    }
                    (u64::from_be_bytes(data[8..16].try_into().unwrap()), 16)
                },
                // Runs to the end of the stream: nothing after it to parse.
                0 => {
                    self.skip = u64::MAX;
                    return self.pending.len();
                },
                size => (size, 8),
            };
            if size < header as u64 {
                // Corrupt; stop parsing this stream rather than misread it.
                self.skip = u64::MAX;
                return self.pending.len();
            }
            match &box_type {
                b"moov" | b"trak" | b"mdia" | b"mvex" | b"moof" => position += header,
                b"traf" => {
                    let Some(Format::Mp4(ref mut state)) = self.format else {
                        unreachable!("parse_mp4 runs for MP4 streams");
                    };
                    state.tfhd_default_duration = None;
                    position += header;
                },
                b"mdhd" | b"trex" | b"tfhd" | b"tfdt" | b"trun" => {
                    let size = size as usize;
                    if data.len() < size {
                        return position;
                    }
                    let body = data[header..size].to_vec();
                    self.mp4_box(&box_type, &body, timestamp_offset);
                    position += size;
                },
                _ => {
                    let available = (data.len() - header) as u64;
                    let payload = size - header as u64;
                    if available >= payload {
                        position += size as usize;
                    } else {
                        self.skip = payload - available;
                        return self.pending.len();
                    }
                },
            }
        }
    }

    fn mp4_box(&mut self, box_type: &[u8; 4], body: &[u8], timestamp_offset: f64) {
        let Some(Format::Mp4(ref mut state)) = self.format else {
            unreachable!("mp4_box runs for MP4 streams");
        };
        // Full boxes: version (1 byte) and flags (3 bytes) come first.
        if body.len() < 4 {
            return;
        }
        let version = body[0];
        let flags = u32::from_be_bytes([0, body[1], body[2], body[3]]);
        let fields = &body[4..];
        let u32_at = |offset: usize| -> Option<u32> {
            fields
                .get(offset..offset + 4)
                .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()))
        };
        match box_type {
            b"mdhd" => {
                // v1: creation (8), modification (8), timescale; v0: 4, 4, timescale.
                let offset = if version == 1 { 16 } else { 8 };
                if let Some(timescale) = u32_at(offset).filter(|timescale| *timescale > 0) {
                    state.timescale = timescale;
                }
            },
            b"trex" => {
                // track_ID, default_sample_description_index, default_sample_duration.
                if let Some(duration) = u32_at(8) {
                    state.trex_default_duration = duration;
                }
            },
            b"tfhd" => {
                let mut offset = 4; // track_ID
                if flags & 0x01 != 0 {
                    offset += 8; // base_data_offset
                }
                if flags & 0x02 != 0 {
                    offset += 4; // sample_description_index
                }
                if flags & 0x08 != 0 {
                    state.tfhd_default_duration = u32_at(offset);
                }
            },
            b"tfdt" => {
                state.next_decode_time = if version == 1 {
                    fields
                        .get(..8)
                        .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
                        .unwrap_or(0)
                } else {
                    u32_at(0).unwrap_or(0) as u64
                };
            },
            b"trun" => {
                let Some(sample_count) = u32_at(0) else {
                    return;
                };
                let mut offset = 4;
                if flags & 0x001 != 0 {
                    offset += 4; // data_offset
                }
                if flags & 0x004 != 0 {
                    offset += 4; // first_sample_flags
                }
                let per_sample = [0x100, 0x200, 0x400, 0x800]
                    .iter()
                    .filter(|flag| flags & **flag != 0)
                    .count() *
                    4;
                let default_duration = state
                    .tfhd_default_duration
                    .unwrap_or(state.trex_default_duration)
                    as u64;
                let mut total = 0u64;
                for sample in 0..sample_count as usize {
                    total += if flags & 0x100 != 0 {
                        u32_at(offset + sample * per_sample).unwrap_or(0) as u64
                    } else {
                        default_duration
                    };
                }
                let timescale = state.timescale as f64;
                let start = state.next_decode_time as f64 / timescale + timestamp_offset;
                let end = (state.next_decode_time + total) as f64 / timescale + timestamp_offset;
                state.next_decode_time += total;
                if end > start {
                    self.add_range(start, end);
                }
            },
            _ => {},
        }
    }
}

fn detect_format(bytes: &[u8]) -> Option<Format> {
    if bytes.len() < 8 {
        return None;
    }
    if bytes[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some(Format::WebM(WebMState::default()));
    }
    if matches!(
        &bytes[4..8],
        b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx" | b"emsg" | b"free"
    ) {
        return Some(Format::Mp4(Mp4State::default()));
    }
    // Neither: leave `format` unset so nothing is misparsed; `buffered` stays empty.
    None
}

/// An EBML element id with its length-marker bits kept.
fn read_ebml_id(data: &[u8]) -> Option<(u64, usize)> {
    let first = *data.first()?;
    let length = first.leading_zeros() as usize + 1;
    if length > 4 || data.len() < length {
        return None;
    }
    Some((
        data[..length]
            .iter()
            .fold(0u64, |value, byte| (value << 8) | *byte as u64),
        length,
    ))
}

/// An EBML size (marker bit removed); `None` for the reserved "unknown size" value.
fn read_ebml_size(data: &[u8]) -> Option<(Option<u64>, usize)> {
    let first = *data.first()?;
    let length = first.leading_zeros() as usize + 1;
    if length > 8 || data.len() < length {
        return None;
    }
    let mask = if length == 8 { 0 } else { 0xFFu8 >> length };
    let mut value = (first & mask) as u64;
    for byte in &data[1..length] {
        value = (value << 8) | *byte as u64;
    }
    let unknown = value == (1u64 << (7 * length)) - 1;
    Some((if unknown { None } else { Some(value) }, length))
}

fn read_uint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .take(8)
        .fold(0u64, |value, byte| (value << 8) | *byte as u64)
}
