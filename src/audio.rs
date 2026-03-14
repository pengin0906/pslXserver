// PulseAudio Native Protocol TCP Server
// Implements minimum viable PA server for Chrome/Firefox audio playback.
// Accepts PULSE_SERVER=tcp:<ip>:4713 connections and plays audio via AudioQueue.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use log::{info, warn, debug};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ============================================================================
// PulseAudio Protocol Constants
// ============================================================================

const PA_COMMAND_ERROR: u32 = 0;
const PA_COMMAND_REPLY: u32 = 2;
const PA_COMMAND_CREATE_PLAYBACK_STREAM: u32 = 3;
const PA_COMMAND_DELETE_PLAYBACK_STREAM: u32 = 4;
const PA_COMMAND_AUTH: u32 = 8;
const PA_COMMAND_SET_CLIENT_NAME: u32 = 9;
const PA_COMMAND_LOOKUP_SINK: u32 = 10;
const PA_COMMAND_DRAIN_PLAYBACK_STREAM: u32 = 12;
const PA_COMMAND_GET_PLAYBACK_LATENCY: u32 = 14;
const PA_COMMAND_GET_SERVER_INFO: u32 = 20;
const PA_COMMAND_GET_SINK_INFO: u32 = 21;
const PA_COMMAND_GET_SINK_INFO_LIST: u32 = 22;
const PA_COMMAND_GET_SOURCE_INFO_LIST: u32 = 24;
const PA_COMMAND_GET_MODULE_INFO_LIST: u32 = 26;
const PA_COMMAND_GET_CLIENT_INFO_LIST: u32 = 28;
const PA_COMMAND_GET_SINK_INPUT_INFO_LIST: u32 = 30;
const PA_COMMAND_GET_SOURCE_OUTPUT_INFO_LIST: u32 = 32;
const PA_COMMAND_GET_SAMPLE_INFO_LIST: u32 = 34;
const PA_COMMAND_SUBSCRIBE: u32 = 35;
// PA native protocol enum order: after SUBSCRIBE come SET_*_VOLUME/MUTE, then stream ops
const PA_COMMAND_CORK_PLAYBACK_STREAM: u32 = 41;
const PA_COMMAND_FLUSH_PLAYBACK_STREAM: u32 = 42;
const PA_COMMAND_TRIGGER_PLAYBACK_STREAM: u32 = 43;
const PA_COMMAND_SET_PLAYBACK_STREAM_NAME: u32 = 46;
// Server → client commands
const PA_COMMAND_REQUEST: u32 = 61;

const PA_PROTOCOL_VERSION: u32 = 16;
const PA_INVALID_INDEX: u32 = 0xFFFFFFFF;
const PA_CHANNEL_COMMAND: u32 = 0xFFFFFFFF;
const PA_PSTREAM_DESCRIPTOR_SIZE: usize = 20;

// Tagstruct type tags
const TAG_STRING: u8 = b't';
const TAG_STRING_NULL: u8 = b'N';
const TAG_U32: u8 = b'L';
const TAG_U8: u8 = b'B';
const TAG_S64: u8 = b'r'; // PA_TAG_S64 = 'r' (lowercase), PA_TAG_U64 = 'R' (uppercase)
const TAG_SAMPLE_SPEC: u8 = b'a';
const TAG_CHANNEL_MAP: u8 = b'm';
const TAG_CVOLUME: u8 = b'v';
const TAG_PROPLIST: u8 = b'P';
const TAG_BOOLEAN_TRUE: u8 = b'1';
const TAG_BOOLEAN_FALSE: u8 = b'0';
const TAG_ARBITRARY: u8 = b'x';
const TAG_USEC: u8 = b'U';
const TAG_VOLUME: u8 = b'V';
const TAG_TIMEVAL: u8 = b'T';

// Sample formats
const PA_SAMPLE_S16LE: u8 = 3;

// ============================================================================
// Tagstruct Writer
// ============================================================================

struct TagWriter {
    buf: Vec<u8>,
}

impl TagWriter {
    fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }

    fn put_u32(&mut self, val: u32) {
        self.buf.push(TAG_U32);
        self.buf.extend_from_slice(&val.to_be_bytes());
    }

    fn put_u8(&mut self, val: u8) {
        self.buf.push(TAG_U8);
        self.buf.push(val);
    }

    fn put_string(&mut self, s: &str) {
        self.buf.push(TAG_STRING);
        self.buf.extend_from_slice(s.as_bytes());
        self.buf.push(0);
    }

    fn put_string_null(&mut self) {
        self.buf.push(TAG_STRING_NULL);
    }

    fn put_sample_spec(&mut self, format: u8, channels: u8, rate: u32) {
        self.buf.push(TAG_SAMPLE_SPEC);
        self.buf.push(format);
        self.buf.push(channels);
        self.buf.extend_from_slice(&rate.to_be_bytes());
    }

    fn put_channel_map(&mut self, channels: u8) {
        self.buf.push(TAG_CHANNEL_MAP);
        self.buf.push(channels);
        // Stereo: left=1, right=2
        if channels >= 1 { self.buf.push(1); }
        if channels >= 2 { self.buf.push(2); }
        for _ in 2..channels {
            self.buf.push(0);
        }
    }

    fn put_cvolume(&mut self, channels: u8, volume: u32) {
        self.buf.push(TAG_CVOLUME);
        self.buf.push(channels);
        for _ in 0..channels {
            self.buf.extend_from_slice(&volume.to_be_bytes());
        }
    }

    fn put_usec(&mut self, usec: u64) {
        self.buf.push(TAG_USEC);
        self.buf.extend_from_slice(&usec.to_be_bytes());
    }

    fn put_bool(&mut self, val: bool) {
        self.buf.push(if val { TAG_BOOLEAN_TRUE } else { TAG_BOOLEAN_FALSE });
    }

    fn put_arbitrary(&mut self, data: &[u8]) {
        self.buf.push(TAG_ARBITRARY);
        self.buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.buf.extend_from_slice(data);
    }

    fn put_timeval(&mut self, tv_sec: u32, tv_usec: u32) {
        self.buf.push(TAG_TIMEVAL);
        self.buf.extend_from_slice(&tv_sec.to_be_bytes());
        self.buf.extend_from_slice(&tv_usec.to_be_bytes());
    }

    fn put_proplist_empty(&mut self) {
        self.buf.push(TAG_PROPLIST);
        self.buf.push(TAG_STRING_NULL); // empty proplist terminated by null string
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

// ============================================================================
// Tagstruct Reader
// ============================================================================

struct TagReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> TagReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn peek_tag(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    fn read_u32(&mut self) -> Option<u32> {
        if self.data.get(self.pos)? != &TAG_U32 { return None; }
        self.pos += 1;
        if self.pos + 4 > self.data.len() { return None; }
        let val = u32::from_be_bytes(self.data[self.pos..self.pos+4].try_into().ok()?);
        self.pos += 4;
        Some(val)
    }

    fn read_string(&mut self) -> Option<String> {
        match self.data.get(self.pos)? {
            &TAG_STRING => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.data.len() && self.data[self.pos] != 0 {
                    self.pos += 1;
                }
                let s = std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("").to_string();
                self.pos += 1; // skip null terminator
                Some(s)
            }
            &TAG_STRING_NULL => {
                self.pos += 1;
                Some(String::new())
            }
            _ => None,
        }
    }

    fn read_arbitrary(&mut self) -> Option<Vec<u8>> {
        if self.data.get(self.pos)? != &TAG_ARBITRARY { return None; }
        self.pos += 1;
        if self.pos + 4 > self.data.len() { return None; }
        let len = u32::from_be_bytes(self.data[self.pos..self.pos+4].try_into().ok()?) as usize;
        self.pos += 4;
        if self.pos + len > self.data.len() { return None; }
        let data = self.data[self.pos..self.pos+len].to_vec();
        self.pos += len;
        Some(data)
    }

    fn read_bool(&mut self) -> Option<bool> {
        match self.data.get(self.pos)? {
            &TAG_BOOLEAN_TRUE => { self.pos += 1; Some(true) }
            &TAG_BOOLEAN_FALSE => { self.pos += 1; Some(false) }
            _ => None,
        }
    }

    fn read_sample_spec(&mut self) -> Option<(u8, u8, u32)> {
        if self.data.get(self.pos)? != &TAG_SAMPLE_SPEC { return None; }
        self.pos += 1;
        if self.pos + 6 > self.data.len() { return None; }
        let format = self.data[self.pos]; self.pos += 1;
        let channels = self.data[self.pos]; self.pos += 1;
        let rate = u32::from_be_bytes(self.data[self.pos..self.pos+4].try_into().ok()?);
        self.pos += 4;
        Some((format, channels, rate))
    }

    fn read_channel_map(&mut self) -> Option<Vec<u8>> {
        if self.data.get(self.pos)? != &TAG_CHANNEL_MAP { return None; }
        self.pos += 1;
        if self.pos >= self.data.len() { return None; }
        let channels = self.data[self.pos] as usize; self.pos += 1;
        if self.pos + channels > self.data.len() { return None; }
        let map = self.data[self.pos..self.pos+channels].to_vec();
        self.pos += channels;
        Some(map)
    }

    fn read_cvolume(&mut self) -> Option<Vec<u32>> {
        if self.data.get(self.pos)? != &TAG_CVOLUME { return None; }
        self.pos += 1;
        if self.pos >= self.data.len() { return None; }
        let channels = self.data[self.pos] as usize; self.pos += 1;
        let mut volumes = Vec::with_capacity(channels);
        for _ in 0..channels {
            if self.pos + 4 > self.data.len() { return None; }
            volumes.push(u32::from_be_bytes(self.data[self.pos..self.pos+4].try_into().ok()?));
            self.pos += 4;
        }
        Some(volumes)
    }

    fn read_proplist(&mut self) -> Option<Vec<(String, Vec<u8>)>> {
        if self.data.get(self.pos)? != &TAG_PROPLIST { return None; }
        self.pos += 1;
        let mut props = Vec::new();
        loop {
            match self.data.get(self.pos) {
                Some(&TAG_STRING_NULL) => {
                    self.pos += 1;
                    break;
                }
                Some(&TAG_STRING) => {
                    self.pos += 1;
                    let start = self.pos;
                    while self.pos < self.data.len() && self.data[self.pos] != 0 {
                        self.pos += 1;
                    }
                    let key = std::str::from_utf8(&self.data[start..self.pos]).unwrap_or("").to_string();
                    self.pos += 1; // null term

                    // Read length (TAG_U32)
                    let len = self.read_u32()? as usize;
                    // Read value (TAG_ARBITRARY)
                    let val = self.read_arbitrary()?;
                    let _ = len; // length is redundant
                    props.push((key, val));
                }
                _ => break,
            }
        }
        Some(props)
    }

    /// Skip any single tagstruct value (used to skip fields we don't care about)
    fn skip_value(&mut self) -> bool {
        if self.pos >= self.data.len() { return false; }
        match self.data[self.pos] {
            TAG_U32 => { self.pos += 5; true }
            TAG_U8 => { self.pos += 2; true }
            TAG_STRING => {
                self.pos += 1;
                while self.pos < self.data.len() && self.data[self.pos] != 0 { self.pos += 1; }
                self.pos += 1;
                true
            }
            TAG_STRING_NULL => { self.pos += 1; true }
            TAG_BOOLEAN_TRUE | TAG_BOOLEAN_FALSE => { self.pos += 1; true }
            TAG_SAMPLE_SPEC => { self.pos += 7; true } // tag + format + channels + rate
            TAG_CHANNEL_MAP => {
                self.pos += 1;
                if self.pos >= self.data.len() { return false; }
                let ch = self.data[self.pos] as usize;
                self.pos += 1 + ch;
                true
            }
            TAG_CVOLUME => {
                self.pos += 1;
                if self.pos >= self.data.len() { return false; }
                let ch = self.data[self.pos] as usize;
                self.pos += 1 + ch * 4;
                true
            }
            TAG_PROPLIST => {
                let _ = self.read_proplist();
                true
            }
            TAG_ARBITRARY => {
                self.pos += 1;
                if self.pos + 4 > self.data.len() { return false; }
                let len = u32::from_be_bytes(self.data[self.pos..self.pos+4].try_into().unwrap_or([0;4])) as usize;
                self.pos += 4 + len;
                true
            }
            TAG_USEC => { self.pos += 9; true }
            TAG_S64 => { self.pos += 9; true }
            TAG_VOLUME => { self.pos += 5; true }
            _ => {
                debug!("PA: unknown tag 0x{:02x} at pos {}", self.data[self.pos], self.pos);
                false
            }
        }
    }
}

// ============================================================================
// Packet I/O
// ============================================================================

async fn read_packet(stream: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<(u32, u32, Vec<u8>)> {
    let mut desc = [0u8; PA_PSTREAM_DESCRIPTOR_SIZE];
    stream.read_exact(&mut desc).await?;

    let length = u32::from_be_bytes(desc[0..4].try_into().unwrap());
    let channel = u32::from_be_bytes(desc[4..8].try_into().unwrap());

    // Debug: log every descriptor for first few packets
    static PKT_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = PKT_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 20 || length > 1024 * 1024 {
        pa_log(&format!("PA: pkt#{} len={} ch={} desc={:02x?}", n, length, channel, &desc));
    }
    // offset_hi, offset_lo, flags — ignored for commands

    let flags = u32::from_be_bytes(desc[16..20].try_into().unwrap());

    // Handle SHM release/revoke frames (no payload per PA spec)
    if flags == 0x40000000 || flags == 0xC0000000 {
        return Ok((channel, 0, Vec::new()));
    }

    // Handle SHM data reference frames
    if (flags & 0x80000000) != 0 {
        if length > 0 && length <= 1024 {
            let mut skip_buf = vec![0u8; length as usize];
            stream.read_exact(&mut skip_buf).await?;
        }
        return Ok((channel, 0, Vec::new()));
    }

    // PA spec: FRAME_SIZE_MAX_ALLOW = 16MB
    if length > 16 * 1024 * 1024 {
        pa_log(&format!("PA: invalid frame len={} flags=0x{:08x} ch={} desc={:02x?}",
            length, flags, channel, desc));
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "packet too large"));
    }

    let mut payload = vec![0u8; length as usize];
    if length > 0 {
        stream.read_exact(&mut payload).await?;
    }

    Ok((channel, length, payload))
}

async fn write_packet(stream: &mut (impl AsyncWriteExt + Unpin), channel: u32, payload: &[u8]) -> std::io::Result<()> {
    let mut desc = [0u8; PA_PSTREAM_DESCRIPTOR_SIZE];
    desc[0..4].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    desc[4..8].copy_from_slice(&channel.to_be_bytes());
    // offset and flags = 0

    static WRITE_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = WRITE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 20 {
        pa_log(&format!("PA: WRITE#{} len={} ch={} desc={:02x?}", n, payload.len(), channel, &desc));
    }

    // Send descriptor + payload as single write to avoid TCP fragmentation
    let mut buf = Vec::with_capacity(PA_PSTREAM_DESCRIPTOR_SIZE + payload.len());
    buf.extend_from_slice(&desc);
    buf.extend_from_slice(payload);
    stream.write_all(&buf).await?;
    stream.flush().await?;
    Ok(())
}

fn make_reply(tag: u32, writer: &mut TagWriter) -> Vec<u8> {
    let mut reply = TagWriter::new();
    reply.put_u32(PA_COMMAND_REPLY);
    reply.put_u32(tag);
    reply.buf.extend_from_slice(&writer.buf);
    reply.into_bytes()
}

fn make_simple_reply(tag: u32) -> Vec<u8> {
    let mut w = TagWriter::new();
    w.put_u32(PA_COMMAND_REPLY);
    w.put_u32(tag);
    w.into_bytes()
}

fn make_error(tag: u32, error_code: u32) -> Vec<u8> {
    let mut w = TagWriter::new();
    w.put_u32(PA_COMMAND_ERROR);
    w.put_u32(tag);
    w.put_u32(error_code);
    w.into_bytes()
}

// ============================================================================
// Ring Buffer (shared between PA server and AudioQueue callback)
// ============================================================================

pub struct AudioRingBuffer {
    data: VecDeque<u8>,
    capacity: usize,
}

const RING_CAPACITY: usize = 48000 * 4 * 2; // ~2 seconds at 48kHz stereo s16le

impl AudioRingBuffer {
    pub fn new() -> Self {
        Self {
            data: VecDeque::with_capacity(RING_CAPACITY),
            capacity: RING_CAPACITY,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let available = self.capacity.saturating_sub(self.data.len());
        if bytes.len() > available {
            let to_drop = bytes.len() - available;
            self.data.drain(..to_drop);
        }
        self.data.extend(bytes);
    }

    pub fn pull(&mut self, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.data.len());
        for (i, byte) in self.data.drain(..n).enumerate() {
            buf[i] = byte;
        }
        n
    }
}

pub static AUDIO_RING: std::sync::Mutex<Option<AudioRingBuffer>> = std::sync::Mutex::new(None);
pub static AUDIO_RUNNING: AtomicBool = AtomicBool::new(false);
pub static AUDIO_READY_TO_INIT: AtomicBool = AtomicBool::new(false);

// Stream format from CREATE_PLAYBACK_STREAM (set by PA handler, read by check_audio_init)
static AUDIO_FORMAT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(3); // PA_SAMPLE_S16LE
static AUDIO_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(48000);
static AUDIO_CHANNELS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

const PA_SAMPLE_FLOAT32LE: u8 = 5;
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;

// Store AudioQueueRef globally for dispose on re-init
static AUDIO_QUEUE_REF: std::sync::Mutex<Option<usize>> = std::sync::Mutex::new(None);

// Track bytes consumed by AudioQueue callback (for REQUEST flow control)
static AUDIO_BYTES_CONSUMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

// ============================================================================
// AudioQueue FFI (macOS + iOS)
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: u32,
    m_format_flags: u32,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    m_reserved: u32,
}

const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6C70636D;
const K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER: u32 = 4;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 8;

type AudioQueueRef = *mut c_void;
type AudioQueueBufferRef = *mut AudioQueueBuffer;

#[repr(C)]
struct AudioQueueBuffer {
    m_audio_data_bytes_capacity: u32,
    m_audio_data: *mut c_void,
    m_audio_data_byte_size: u32,
    m_user_data: *mut c_void,
    m_packet_description_capacity: u32,
    m_packet_descriptions: *mut c_void,
    m_packet_description_count: u32,
}

type AudioQueueOutputCallback = unsafe extern "C" fn(
    in_user_data: *mut c_void,
    in_aq: AudioQueueRef,
    in_buffer: AudioQueueBufferRef,
);

#[cfg_attr(target_os = "macos", link(name = "AudioToolbox", kind = "framework"))]
extern "C" {
    fn AudioQueueNewOutput(
        in_format: *const AudioStreamBasicDescription,
        in_callback_proc: AudioQueueOutputCallback,
        in_user_data: *mut c_void,
        in_callback_run_loop: *mut c_void,
        in_callback_run_loop_mode: *mut c_void,
        in_flags: u32,
        out_aq: *mut AudioQueueRef,
    ) -> i32;

    fn AudioQueueAllocateBuffer(
        in_aq: AudioQueueRef,
        in_buffer_byte_size: u32,
        out_buffer: *mut AudioQueueBufferRef,
    ) -> i32;

    fn AudioQueueEnqueueBuffer(
        in_aq: AudioQueueRef,
        in_buffer: AudioQueueBufferRef,
        in_num_packet_descriptions: u32,
        in_packet_descs: *const c_void,
    ) -> i32;

    fn AudioQueueStart(
        in_aq: AudioQueueRef,
        in_start_time: *const c_void,
    ) -> i32;

    fn AudioQueueStop(
        in_aq: AudioQueueRef,
        in_immediate: u8,
    ) -> i32;

    fn AudioQueueDispose(
        in_aq: AudioQueueRef,
        in_immediate: u8,
    ) -> i32;
}

const AQ_FRAMES_PER_BUFFER: u32 = 4096;
const AQ_BYTES_PER_FRAME: u32 = 4; // stereo s16le
const AQ_BUFFER_BYTES: u32 = AQ_FRAMES_PER_BUFFER * AQ_BYTES_PER_FRAME;
const AQ_NUM_BUFFERS: usize = 3;

unsafe extern "C" fn audio_queue_callback(
    _in_user_data: *mut c_void,
    in_aq: AudioQueueRef,
    in_buffer: AudioQueueBufferRef,
) {
    let buf = &mut *in_buffer;
    let capacity = buf.m_audio_data_bytes_capacity as usize;
    let audio_data = std::slice::from_raw_parts_mut(buf.m_audio_data as *mut u8, capacity);

    let filled = if let Ok(mut ring) = AUDIO_RING.lock() {
        if let Some(ref mut rb) = *ring {
            rb.pull(audio_data)
        } else { 0 }
    } else { 0 };

    if filled < capacity {
        audio_data[filled..].fill(0);
    }
    if filled > 0 {
        AUDIO_BYTES_CONSUMED.fetch_add(filled as u64, Ordering::Relaxed);
    }
    buf.m_audio_data_byte_size = capacity as u32;
    AudioQueueEnqueueBuffer(in_aq, in_buffer, 0, std::ptr::null());
}

/// Initialize AudioQueue. Call from main thread on iOS, any thread on macOS.
/// pa_format: 3=s16le, 5=float32le
pub unsafe fn init_audio_queue(sample_rate: u32, channels: u8, pa_format: u8) -> Option<AudioQueueRef> {
    let (bits, flags, bpf) = if pa_format == PA_SAMPLE_FLOAT32LE {
        (32u32, K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED, channels as u32 * 4)
    } else {
        (16u32, K_AUDIO_FORMAT_FLAG_IS_SIGNED_INTEGER | K_AUDIO_FORMAT_FLAG_IS_PACKED, channels as u32 * 2)
    };
    let format = AudioStreamBasicDescription {
        m_sample_rate: sample_rate as f64,
        m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
        m_format_flags: flags,
        m_bytes_per_packet: bpf,
        m_frames_per_packet: 1,
        m_bytes_per_frame: bpf,
        m_channels_per_frame: channels as u32,
        m_bits_per_channel: bits,
        m_reserved: 0,
    };

    let buf_bytes = AQ_FRAMES_PER_BUFFER * bpf;
    let mut queue: AudioQueueRef = std::ptr::null_mut();
    let status = AudioQueueNewOutput(
        &format, audio_queue_callback,
        std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(),
        0, &mut queue,
    );
    if status != 0 {
        log::error!("AudioQueueNewOutput failed: {}", status);
        return None;
    }

    for _ in 0..AQ_NUM_BUFFERS {
        let mut buffer: AudioQueueBufferRef = std::ptr::null_mut();
        if AudioQueueAllocateBuffer(queue, buf_bytes, &mut buffer) != 0 {
            AudioQueueDispose(queue, 1);
            return None;
        }
        let b = &mut *buffer;
        std::ptr::write_bytes(b.m_audio_data as *mut u8, 0, b.m_audio_data_bytes_capacity as usize);
        b.m_audio_data_byte_size = b.m_audio_data_bytes_capacity;
        if AudioQueueEnqueueBuffer(queue, buffer, 0, std::ptr::null()) != 0 {
            AudioQueueDispose(queue, 1);
            return None;
        }
    }

    if AudioQueueStart(queue, std::ptr::null()) != 0 {
        AudioQueueDispose(queue, 1);
        return None;
    }

    let fmt_name = if pa_format == PA_SAMPLE_FLOAT32LE { "float32le" } else { "s16le" };
    info!("AudioQueue started: {}Hz {}ch {}", sample_rate, channels, fmt_name);
    pa_log(&format!("AudioQueue started: {}Hz {}ch {}", sample_rate, channels, fmt_name));
    Some(queue)
}

/// Configure AVAudioSession (iOS only)
#[cfg(target_os = "ios")]
pub unsafe fn configure_audio_session() {
    use objc2::msg_send;
    use objc2::runtime::{AnyObject, AnyClass, Bool};
    use objc2_foundation::NSString;

    let cls: *const AnyClass = objc2::class!(AVAudioSession);
    let session: *mut AnyObject = msg_send![cls, sharedInstance];
    if session.is_null() { return; }

    let category = NSString::from_str("AVAudioSessionCategoryPlayback");
    let mut error: *mut AnyObject = std::ptr::null_mut();
    let _: Bool = msg_send![session, setCategory: &*category error: &mut error];
    let _: Bool = msg_send![session, setActive: Bool::YES error: &mut error];
    info!("AVAudioSession configured for playback");
}

#[cfg(target_os = "macos")]
pub unsafe fn configure_audio_session() {
    // No-op on macOS — CoreAudio handles routing
}

/// Called from main thread timer to initialize AudioQueue when ready.
pub unsafe fn check_audio_init() {
    if !AUDIO_READY_TO_INIT.load(Ordering::SeqCst) || AUDIO_RUNNING.load(Ordering::SeqCst) {
        return;
    }

    info!("Audio: initializing AudioQueue on main thread");
    configure_audio_session();

    // Initialize ring buffer
    {
        let mut ring = AUDIO_RING.lock().unwrap();
        if ring.is_none() {
            *ring = Some(AudioRingBuffer::new());
        }
    }

    // Dispose old AudioQueue if re-initializing
    {
        let mut aq = AUDIO_QUEUE_REF.lock().unwrap();
        if let Some(old_ref) = aq.take() {
            pa_log("Audio: disposing old AudioQueue for re-init");
            AudioQueueStop(old_ref as AudioQueueRef, 1);
            AudioQueueDispose(old_ref as AudioQueueRef, 1);
        }
    }

    let rate = AUDIO_RATE.load(Ordering::SeqCst);
    let channels = AUDIO_CHANNELS.load(Ordering::SeqCst);
    let pa_fmt = AUDIO_FORMAT.load(Ordering::SeqCst);
    pa_log(&format!("Audio: init fmt={} rate={} ch={}", pa_fmt, rate, channels));
    if let Some(queue) = init_audio_queue(rate, channels, pa_fmt) {
        *AUDIO_QUEUE_REF.lock().unwrap() = Some(queue as usize);
        AUDIO_RUNNING.store(true, Ordering::SeqCst);
        info!("Audio: AudioQueue initialized and running");
    } else {
        log::error!("Audio: failed to initialize AudioQueue");
        AUDIO_READY_TO_INIT.store(false, Ordering::SeqCst);
    }
}

// ============================================================================
// PulseAudio Command Handlers
// ============================================================================

fn handle_auth(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    // Read client protocol version
    let client_version = reader.read_u32().unwrap_or(0);
    let _cookie = reader.read_arbitrary(); // ignore cookie (anonymous auth)
    info!("PA: AUTH from client, version={}", client_version);

    let mut w = TagWriter::new();
    w.put_u32(PA_PROTOCOL_VERSION);
    make_reply(tag, &mut w)
}

fn handle_set_client_name(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    // v13+: proplist
    if let Some(props) = reader.read_proplist() {
        for (k, v) in &props {
            if k == "application.name" {
                let name = String::from_utf8_lossy(v);
                info!("PA: client name: {}", name.trim_end_matches('\0'));
            }
        }
    }

    let mut w = TagWriter::new();
    w.put_u32(0); // client index
    make_reply(tag, &mut w)
}

fn handle_get_server_info(tag: u32) -> Vec<u8> {
    let mut w = TagWriter::new();
    w.put_string("Xerver");         // server name (package name)
    w.put_string("0.1.0");              // server version
    w.put_string("penguin");            // user name
    w.put_string("Xerver");         // hostname
    w.put_sample_spec(PA_SAMPLE_S16LE, 2, 48000); // default sample spec
    w.put_string("pslx_sink");          // default sink name
    w.put_string("pslx_monitor");       // default source name
    w.put_u32(0);                       // cookie
    w.put_channel_map(2);               // default channel map
    make_reply(tag, &mut w)
}

fn handle_get_sink_info_list(tag: u32) -> Vec<u8> {
    let mut w = TagWriter::new();
    // One sink entry
    w.put_u32(0);                       // index
    w.put_string("pslx_sink");          // name
    w.put_string("Xerver Audio Output"); // description
    w.put_sample_spec(PA_SAMPLE_S16LE, 2, 48000);
    w.put_channel_map(2);
    w.put_u32(0);                       // owner module index
    w.put_cvolume(2, 0x10000);          // volume (100% = 0x10000)
    w.put_bool(false);                  // muted
    w.put_u32(0);                       // monitor source index
    w.put_string("pslx_monitor");       // monitor source name
    w.put_usec(0);                      // latency
    w.put_string("module-null-sink");   // driver
    w.put_u32(0);                       // flags
    // v13: proplist
    w.put_proplist_empty();
    w.put_usec(0);                      // configured latency
    // v15: base_volume, state, n_volume_steps, card_index
    w.put_u32(0x10000);                 // base volume
    w.put_u32(0);                       // state: RUNNING = 0
    w.put_u32(0x10001);                 // n_volume_steps
    w.put_u32(PA_INVALID_INDEX);        // card index
    // v16: n_ports, active_port
    w.put_u32(0);                       // n_ports
    w.put_string_null();                // active port (NULL)
    make_reply(tag, &mut w)
}

fn handle_get_sink_info(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    // Read sink_index and/or sink_name — then return same as list
    let _index = reader.read_u32();
    let _name = reader.read_string();
    handle_get_sink_info_list(tag)
}

fn handle_lookup_sink(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    let _name = reader.read_string();
    let mut w = TagWriter::new();
    w.put_u32(0); // sink index
    make_reply(tag, &mut w)
}

fn handle_create_playback_stream(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    // Read stream parameters — field order per PA protocol:
    // sample_spec, channel_map, sink_index, sink_name, maxlength,
    // corked, tlength, prebuf, minreq, sync_id, cvolume
    let ss = reader.read_sample_spec().unwrap_or((PA_SAMPLE_S16LE, 2, 48000));
    let _cm = reader.read_channel_map();
    let _sink_index = reader.read_u32();
    let _sink_name = reader.read_string();
    let _maxlength = reader.read_u32().unwrap_or(0xFFFFFFFF);
    let _corked = reader.read_bool();        // corked comes BEFORE tlength
    let tlength = reader.read_u32().unwrap_or(0xFFFFFFFF);
    let _prebuf = reader.read_u32().unwrap_or(0xFFFFFFFF);
    let _minreq = reader.read_u32().unwrap_or(0);
    let _sync_id = reader.read_u32();
    let _cvolume = reader.read_cvolume();

    let (format, channels, rate) = ss;
    pa_log(&format!("PA: CREATE_PLAYBACK_STREAM fmt={} ch={} rate={} pos_after_base={}", format, channels, rate, reader.pos));

    // v12 fields: no_remap, no_remix, fix_format, fix_rate, fix_channels, no_move, variable_rate
    for _ in 0..7 { let _ = reader.skip_value(); }
    // v13 fields: muted, adjust_latency, proplist
    let _ = reader.skip_value(); // muted
    let _ = reader.skip_value(); // adjust_latency
    let _ = reader.read_proplist(); // proplist
    // v14: early_requests
    let _ = reader.skip_value();
    // v15: muted_set, dont_inhibit_auto_suspend, fail_on_suspend
    for _ in 0..3 { let _ = reader.skip_value(); }
    // v16: relative_volume — not present in v16 server negotiate

    pa_log(&format!("PA: parsed ok, pos={} remaining={}", reader.pos, reader.remaining()));

    // Initialize ring buffer NOW so audio data doesn't get dropped
    {
        let mut ring = AUDIO_RING.lock().unwrap();
        if ring.is_none() {
            *ring = Some(AudioRingBuffer::new());
            pa_log("PA: ring buffer initialized in CREATE_PLAYBACK_STREAM");
        }
    }

    // If AudioQueue already running with different format, force re-init
    if AUDIO_RUNNING.load(Ordering::SeqCst) {
        let old_fmt = AUDIO_FORMAT.load(Ordering::SeqCst);
        let old_rate = AUDIO_RATE.load(Ordering::SeqCst);
        let old_ch = AUDIO_CHANNELS.load(Ordering::SeqCst);
        if old_fmt != format || old_rate != rate || old_ch != channels {
            pa_log(&format!("PA: format changed {}Hz/fmt{}/{}ch -> {}Hz/fmt{}/{}ch, resetting AudioQueue",
                old_rate, old_fmt, old_ch, rate, format, channels));
            AUDIO_RUNNING.store(false, Ordering::SeqCst);
        }
    }

    // Store format for AudioQueue init
    AUDIO_FORMAT.store(format, Ordering::SeqCst);
    AUDIO_RATE.store(rate, Ordering::SeqCst);
    AUDIO_CHANNELS.store(channels, Ordering::SeqCst);

    // Signal AudioQueue init (runs on main thread via timer)
    AUDIO_READY_TO_INIT.store(true, Ordering::SeqCst);

    // Compute buffer attributes
    let bpf = match format {
        3 => channels as u32 * 2,    // s16le: 2 bytes per sample
        5 => channels as u32 * 4,    // float32le: 4 bytes per sample
        _ => channels as u32 * 2,    // default to s16le
    };
    let stream_index: u32 = 1;
    let actual_tlength = if tlength == 0xFFFFFFFF { rate * bpf * 2 } else { tlength }; // ~2 seconds
    let actual_minreq = actual_tlength / 100; // small minreq for frequent small requests
    let actual_prebuf = actual_minreq; // minimal prebuf
    let actual_maxlength = rate * bpf * 4; // ~4 seconds

    let mut w = TagWriter::new();
    w.put_u32(stream_index);            // stream index (channel for audio data)
    w.put_u32(0);                       // sink input index
    w.put_u32(actual_tlength);          // missing (bytes to send initially)
    // v9: buffer attributes only
    w.put_u32(actual_maxlength);        // maxlength
    w.put_u32(actual_tlength);          // tlength
    w.put_u32(actual_prebuf);           // prebuf
    w.put_u32(actual_minreq);           // minreq
    // v12: sample_spec, channel_map, sink info
    w.put_sample_spec(format, channels, rate);
    w.put_channel_map(channels);
    w.put_u32(0);                       // sink index
    w.put_string("pslx_sink");          // sink name
    w.put_bool(false);                  // sink suspended
    // v13: configured latency
    w.put_usec(0);

    pa_log(&format!("PA: reply len={}", w.buf.len() + 10));
    make_reply(tag, &mut w)
}

fn handle_get_playback_latency(reader: &mut TagReader, tag: u32) -> Vec<u8> {
    let _stream_index = reader.read_u32();
    // Read client's timeval (TAG_TIMEVAL: tag + sec + usec)
    let (client_sec, client_usec) = if reader.peek_tag() == Some(TAG_TIMEVAL) {
        reader.pos += 1; // skip tag
        let sec = if reader.pos + 4 <= reader.data.len() {
            let v = u32::from_be_bytes(reader.data[reader.pos..reader.pos+4].try_into().unwrap_or([0;4]));
            reader.pos += 4; v
        } else { 0 };
        let usec = if reader.pos + 4 <= reader.data.len() {
            let v = u32::from_be_bytes(reader.data[reader.pos..reader.pos+4].try_into().unwrap_or([0;4]));
            reader.pos += 4; v
        } else { 0 };
        (sec, usec)
    } else { (0, 0) };

    // Reply per PA spec (stream.c stream_get_timing_info_callback):
    // sink_usec, source_usec, playing, local_timeval, remote_timeval, write_index, read_index
    let mut w = TagWriter::new();
    w.put_usec(10000);                  // sink_usec (10ms)
    w.put_usec(0);                      // source_usec
    w.put_bool(AUDIO_RUNNING.load(std::sync::atomic::Ordering::Relaxed)); // playing
    w.put_timeval(client_sec, client_usec); // local (echo back client's timeval)
    w.put_timeval(client_sec, client_usec); // remote (server's current time — use same for simplicity)
    w.put_s64(0);                       // write_index
    w.put_s64(0);                       // read_index
    // v13+: underrun_for, playing_for (TAG_U64 = 'R')
    w.buf.push(b'R'); w.buf.extend_from_slice(&0u64.to_be_bytes()); // underrun_for
    w.buf.push(b'R'); w.buf.extend_from_slice(&0u64.to_be_bytes()); // playing_for
    make_reply(tag, &mut w)
}

impl TagWriter {
    fn put_s64(&mut self, val: i64) {
        self.buf.push(TAG_S64);
        self.buf.extend_from_slice(&val.to_be_bytes());
    }
}

// ============================================================================
// PA Connection Handler
// ============================================================================

fn pa_log(msg: &str) {
    eprintln!("[pslx-pa] {}", msg);
}

async fn handle_pa_connection(stream: tokio::net::TcpStream, addr: std::net::SocketAddr) {
    pa_log(&format!("PA: new client from {}", addr));
    let _ = stream.set_nodelay(true);

    let (mut rd, mut wr) = tokio::io::split(stream);
    let mut request_timer = tokio::time::interval(std::time::Duration::from_millis(20));
    let mut bytes_acked: u64 = 0;
    let mut has_stream = false; // true after CREATE_PLAYBACK_STREAM
    let mut got_audio_data = false; // true after first audio data packet
    let mut missing_bytes: u64 = 0; // bytes client still needs to send (initial fill)
    let mut received_bytes: u64 = 0; // total audio bytes received

    loop {
        tokio::select! {
            biased;

            result = read_packet(&mut rd) => {
                let (channel, _length, payload) = match result {
                    Ok(p) => p,
                    Err(e) => {
                        pa_log(&format!("PA: read error from {}: {:?}", addr, e));
                        break;
                    }
                };

                if channel == PA_CHANNEL_COMMAND {
                    let mut reader = TagReader::new(&payload);
                    let command = match reader.read_u32() {
                        Some(c) => c,
                        None => { pa_log("PA: empty command packet"); continue; }
                    };
                    let tag = reader.read_u32().unwrap_or(0);
                    pa_log(&format!("PA: cmd={} tag={} payload_len={}", command, tag, payload.len()));

                    let reply = match command {
                        PA_COMMAND_AUTH => handle_auth(&mut reader, tag),
                        PA_COMMAND_SET_CLIENT_NAME => handle_set_client_name(&mut reader, tag),
                        PA_COMMAND_GET_SERVER_INFO => handle_get_server_info(tag),
                        PA_COMMAND_GET_SINK_INFO => handle_get_sink_info(&mut reader, tag),
                        PA_COMMAND_GET_SINK_INFO_LIST => handle_get_sink_info_list(tag),
                        PA_COMMAND_LOOKUP_SINK => handle_lookup_sink(&mut reader, tag),
                        PA_COMMAND_CREATE_PLAYBACK_STREAM => {
                            has_stream = true;
                            got_audio_data = false;
                            received_bytes = 0;
                            bytes_acked = AUDIO_BYTES_CONSUMED.load(Ordering::Relaxed);
                            let reply = handle_create_playback_stream(&mut reader, tag);
                            // Extract missing (requested_bytes) from reply: offset 10+5+5 = field 3
                            // missing = actual_tlength stored in reply
                            missing_bytes = {
                                let bpf_guess = 4u64; // conservative
                                let rate = AUDIO_RATE.load(Ordering::SeqCst) as u64;
                                rate * bpf_guess * 2 // match actual_tlength calc
                            };
                            reply
                        }
                        PA_COMMAND_GET_PLAYBACK_LATENCY => handle_get_playback_latency(&mut reader, tag),
                        PA_COMMAND_DRAIN_PLAYBACK_STREAM => make_simple_reply(tag),
                        PA_COMMAND_DELETE_PLAYBACK_STREAM => { has_stream = false; make_simple_reply(tag) }
                        PA_COMMAND_SUBSCRIBE => make_simple_reply(tag),
                        PA_COMMAND_CORK_PLAYBACK_STREAM => make_simple_reply(tag),
                        PA_COMMAND_FLUSH_PLAYBACK_STREAM => make_simple_reply(tag),
                        PA_COMMAND_TRIGGER_PLAYBACK_STREAM => make_simple_reply(tag),
                        PA_COMMAND_SET_PLAYBACK_STREAM_NAME => make_simple_reply(tag),
                        PA_COMMAND_GET_SOURCE_INFO_LIST
                        | PA_COMMAND_GET_MODULE_INFO_LIST
                        | PA_COMMAND_GET_CLIENT_INFO_LIST
                        | PA_COMMAND_GET_SINK_INPUT_INFO_LIST
                        | PA_COMMAND_GET_SOURCE_OUTPUT_INFO_LIST
                        | PA_COMMAND_GET_SAMPLE_INFO_LIST => make_simple_reply(tag),
                        _ => {
                            pa_log(&format!("PA: unhandled command {} tag={}", command, tag));
                            make_simple_reply(tag)
                        }
                    };

                    if let Err(e) = write_packet(&mut wr, PA_CHANNEL_COMMAND, &reply).await {
                        warn!("PA: write error: {}", e);
                        break;
                    }
                } else if channel == 0 && payload.is_empty() {
                    // Skip empty frames on channel 0 (keepalive/padding)
                    continue;
                } else {
                    // Audio data packet — channel is the stream index
                    if payload.is_empty() { continue; } // skip empty audio frames
                    received_bytes += payload.len() as u64;
                    if !got_audio_data {
                        got_audio_data = true;
                        bytes_acked = AUDIO_BYTES_CONSUMED.load(Ordering::Relaxed);
                        pa_log(&format!("PA: first audio data, len={}", payload.len()));
                    }
                    if let Ok(mut ring) = AUDIO_RING.lock() {
                        if let Some(ref mut rb) = *ring {
                            rb.push(&payload);
                        }
                    }
                }
            }

            _ = request_timer.tick(), if has_stream && got_audio_data && received_bytes >= missing_bytes => {
                // Send REQUEST only after initial missing bytes received.
                let request_bytes = if AUDIO_RUNNING.load(Ordering::Relaxed) {
                    let consumed = AUDIO_BYTES_CONSUMED.load(Ordering::Relaxed);
                    if consumed > bytes_acked {
                        let delta = consumed - bytes_acked;
                        bytes_acked = consumed;
                        delta as u32
                    } else {
                        // AudioQueue running but nothing consumed yet — small keepalive request
                        4096
                    }
                } else {
                    // AudioQueue not initialized yet — keep requesting to prevent client timeout
                    4096
                };

                if request_bytes > 0 {
                    let mut w = TagWriter::new();
                    w.put_u32(PA_COMMAND_REQUEST);
                    w.put_u32(0xFFFFFFFF); // tag = -1 (server-initiated)
                    w.put_u32(1);           // stream_index
                    w.put_u32(request_bytes);
                    if let Err(e) = write_packet(&mut wr, PA_CHANNEL_COMMAND, &w.into_bytes()).await {
                        pa_log(&format!("PA: REQUEST write error: {}", e));
                        break;
                    }
                }
            }
        }
    }

    info!("PA: client {} disconnected", addr);
}

// ============================================================================
// PA Server Entry Point
// ============================================================================

/// Start PulseAudio TCP server on port 4713.
/// Spawns as a tokio task — call from within the tokio runtime.
pub async fn start_pulse_server(port: u16) {
    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            log::error!("PA: failed to bind port {}: {}", port, e);
            return;
        }
    };
    info!("PulseAudio TCP server listening on port {}", port);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tokio::spawn(handle_pa_connection(stream, addr));
            }
            Err(e) => {
                log::error!("PA: accept error: {}", e);
            }
        }
    }
}
