//! macOS host glue (MacVR). macOS has no output-loopback capture or virtual microphone that cpal can
//! open, so the host app captures game audio itself (ScreenCaptureKit) and pushes it here, and drains
//! the headset microphone from here into its own audio driver. Also lets the host apply settings and
//! trust clients without the dashboard.

use crate::SESSION_MANAGER;
use alvr_common::{once_cell::sync::Lazy, parking_lot::Mutex};
use alvr_packets::{parse_path, ClientListAction, PathValuePair};
use alvr_sockets::StreamSender;
use std::{
    collections::VecDeque,
    ffi::{c_char, CStr, CString},
    ptr,
    sync::Arc,
};

pub const GAME_AUDIO_SAMPLE_RATE: u32 = 48_000;

pub static GAME_AUDIO_SENDER: Lazy<Mutex<Option<StreamSender<()>>>> = Lazy::new(|| Mutex::new(None));
pub static MICROPHONE_BUFFER: Lazy<Arc<Mutex<VecDeque<f32>>>> =
    Lazy::new(|| Arc::new(Mutex::new(VecDeque::new())));
pub static LAST_STATISTICS: Lazy<Mutex<Option<serde_json::Value>>> = Lazy::new(|| Mutex::new(None));
pub static MICROPHONE_SAMPLE_RATE: Lazy<Mutex<u32>> = Lazy::new(|| Mutex::new(0));

/// Interleaved stereo s16 at 48 kHz. Dropped while no client is streaming.
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_send_game_audio(samples: *const i16, frames: u32) {
    if samples.is_null() || frames == 0 {
        return;
    }
    let Some(sender) = &mut *GAME_AUDIO_SENDER.lock() else {
        return;
    };
    let bytes = std::slice::from_raw_parts(samples as *const u8, frames as usize * 4);
    if let Ok(mut buffer) = sender.get_buffer(&()) {
        buffer.get_range_mut(0, bytes.len()).copy_from_slice(bytes);
        sender.send(buffer).ok();
    }
}

/// Sample rate of the headset microphone stream, 0 while none is streaming.
#[no_mangle]
pub extern "C" fn alvr_macvr_microphone_sample_rate() -> u32 {
    *MICROPHONE_SAMPLE_RATE.lock()
}

/// Copies up to `max` mono f32 microphone samples into `out`; returns how many were copied.
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_read_microphone(out: *mut f32, max: u32) -> u32 {
    if out.is_null() {
        return 0;
    }
    let mut buffer = MICROPHONE_BUFFER.lock();
    let n = buffer.len().min(max as usize);
    for (i, sample) in buffer.drain(..n).enumerate() {
        *out.add(i) = sample;
    }
    n as u32
}

/// Applies `[{"path": "session_settings.video.preferred_codec.variant", "value": "Hevc"}, ...]`.
/// Returns false (and logs) if the JSON or a path is invalid.
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_set_values(json: *const c_char) -> bool {
    let Ok(text) = CStr::from_ptr(json).to_str() else {
        return false;
    };
    let Ok(pairs) = serde_json::from_str::<Vec<serde_json::Value>>(text) else {
        alvr_common::error!("alvr_macvr_set_values: invalid JSON");
        return false;
    };
    let descs = pairs
        .into_iter()
        .filter_map(|p| {
            Some(PathValuePair {
                path: parse_path(p.get("path")?.as_str()?),
                value: p.get("value")?.clone(),
            })
        })
        .collect();
    match SESSION_MANAGER.write().set_values(descs) {
        Ok(()) => true,
        Err(e) => {
            alvr_common::error!("alvr_macvr_set_values: {e:?}");
            false
        }
    }
}

/// JSON object {hostname: {"display_name", "current_ip", "trusted", ...}} of known clients.
/// Call with a null buffer to get the size (including the terminator).
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_client_list_json(buffer: *mut c_char) -> u64 {
    let json = serde_json::to_string(&SESSION_MANAGER.read().session().client_connections)
        .unwrap_or_else(|_| "{}".into());
    let cstring = CString::new(json).unwrap_or_default();
    if !buffer.is_null() {
        ptr::copy_nonoverlapping(cstring.as_ptr(), buffer, cstring.as_bytes_with_nul().len());
    }
    cstring.as_bytes_with_nul().len() as u64
}

/// Trusts a discovered client (as the dashboard's "Trust" button does).
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_trust_client(hostname: *const c_char) {
    if let Ok(hostname) = CStr::from_ptr(hostname).to_str() {
        SESSION_MANAGER
            .write()
            .update_client_list(hostname.to_owned(), ClientListAction::Trust);
    }
}

/// Adds a client by hostname and IP, trusted (wired/USB setup).
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_add_client(hostname: *const c_char, ip: *const c_char) {
    let (Ok(hostname), Ok(ip)) = (CStr::from_ptr(hostname).to_str(), CStr::from_ptr(ip).to_str()) else {
        return;
    };
    let Ok(ip) = ip.parse() else { return };
    SESSION_MANAGER.write().update_client_list(
        hostname.to_owned(),
        ClientListAction::AddIfMissing { trusted: true, manual_ips: vec![ip] },
    );
}

/// The negotiated stream config (eye_resolution_width/height, refresh_rate, codec,
/// enable_foveated_encoding + foveation params, ...) as JSON. It is updated when a client connects;
/// a change makes the client reconnect (no SteamVR restart on macOS). Null buffer = size query.
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_stream_config_json(buffer: *mut c_char) -> u64 {
    let json = serde_json::to_string(&SESSION_MANAGER.read().session().openvr_config)
        .unwrap_or_else(|_| "{}".into());
    let cstring = CString::new(json).unwrap_or_default();
    if !buffer.is_null() {
        ptr::copy_nonoverlapping(cstring.as_ptr(), buffer, cstring.as_bytes_with_nul().len());
    }
    cstring.as_bytes_with_nul().len() as u64
}

/// The latest per-frame statistics (total_latency_ms, encoder_ms, network_ms, decoder_ms, decoder_queue_ms,
/// client_fps, server_fps, bitrate_mbps, packets_lost_total) as JSON, "{}" before the first frame.
/// Needs alvr_report_present/alvr_report_composed per frame. Null buffer = size query.
#[no_mangle]
pub unsafe extern "C" fn alvr_macvr_statistics_json(buffer: *mut c_char) -> u64 {
    let json = LAST_STATISTICS.lock().as_ref().map_or_else(|| "{}".into(), |v| v.to_string());
    let cstring = CString::new(json).unwrap_or_default();
    if !buffer.is_null() {
        ptr::copy_nonoverlapping(cstring.as_ptr(), buffer, cstring.as_bytes_with_nul().len());
    }
    cstring.as_bytes_with_nul().len() as u64
}
