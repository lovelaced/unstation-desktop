//! Android camera-publish access-unit intake (M4, piece 2 — Rust half).
//!
//! The Kotlin capture plugin drives CameraX → a hardware `MediaCodec` H.264 encoder and pushes
//! the encoded access units here over JNII (not the JSON `invoke` bridge — that would base64 +
//! serialize every frame). `start_publish`'s Android feeder drains them through
//! [`segmenter::FragmentBuilder`] into the same mesh path the desktop's ffmpeg feeder uses.
//!
//! Contract with the Kotlin side (`io.parity.unstation.android.CameraBridge`):
//!   1. On the encoder's first output, call `nativeConfig(sps, pps, width, height)` — the
//!      codec-specific data (raw SPS/PPS NAL payloads) needed to build the CMAF init.
//!   2. Per encoded frame, call `nativeVideoAu(annexBytes, ptsUs, keyframe)`.
//! Both are no-ops until `start_publish` has opened a stream, and after `stop_publish` closes it.

use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// H.264 codec-specific data + display size, learned from the encoder's first output.
pub struct CameraConfig {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u16,
    pub height: u16,
}

/// One encoded H.264 access unit (Annex-B framed) with its presentation time.
pub struct CapturedUnit {
    pub data: Vec<u8>,
    pub pts_us: i64,
    pub keyframe: bool,
}

struct Intake {
    config: Option<CameraConfig>,
    tx: Option<UnboundedSender<CapturedUnit>>,
}

fn intake() -> &'static Mutex<Intake> {
    static INTAKE: OnceLock<Mutex<Intake>> = OnceLock::new();
    INTAKE.get_or_init(|| Mutex::new(Intake { config: None, tx: None }))
}

/// `start_publish`: install a fresh AU channel and return its receiver. Clears any stale config
/// from a previous session so we wait for THIS capture's SPS/PPS.
pub fn open_stream() -> UnboundedReceiver<CapturedUnit> {
    let (tx, rx) = unbounded_channel();
    let mut g = intake().lock().unwrap_or_else(|e| e.into_inner());
    g.config = None;
    g.tx = Some(tx);
    rx
}

/// Take the encoder's config once the Kotlin side has reported it (`None` until then).
pub fn take_config() -> Option<CameraConfig> {
    intake().lock().unwrap_or_else(|e| e.into_inner()).config.take()
}

/// `stop_publish`: drop the channel + config so JNI pushes become no-ops.
pub fn close_stream() {
    let mut g = intake().lock().unwrap_or_else(|e| e.into_inner());
    g.tx = None;
    g.config = None;
}

// ---- JNI entry points (called by the Kotlin `CameraBridge`) ---------------------------
//
// Exported from the app's cdylib (`libunstation_android_lib.so`); the Kotlin side declares
// them as `external fun` and loads that library. Android-only.
#[cfg(target_os = "android")]
mod jni_bridge {
    use super::{intake, CameraConfig, CapturedUnit};
    use jni::objects::{JByteArray, JClass};
    use jni::sys::{jboolean, jint, jlong};
    use jni::JNIEnv;

    #[no_mangle]
    pub extern "system" fn Java_io_parity_unstation_android_CameraBridge_nativeConfig(
        env: JNIEnv,
        _class: JClass,
        sps: JByteArray,
        pps: JByteArray,
        width: jint,
        height: jint,
    ) {
        let sps = env.convert_byte_array(&sps).unwrap_or_default();
        let pps = env.convert_byte_array(&pps).unwrap_or_default();
        let mut g = intake().lock().unwrap_or_else(|e| e.into_inner());
        g.config = Some(CameraConfig { sps, pps, width: width as u16, height: height as u16 });
    }

    #[no_mangle]
    pub extern "system" fn Java_io_parity_unstation_android_CameraBridge_nativeVideoAu(
        env: JNIEnv,
        _class: JClass,
        data: JByteArray,
        pts_us: jlong,
        keyframe: jboolean,
    ) {
        let data = match env.convert_byte_array(&data) {
            Ok(d) => d,
            Err(_) => return,
        };
        let g = intake().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = &g.tx {
            let _ = tx.send(CapturedUnit { data, pts_us, keyframe: keyframe != 0 });
        }
    }
}
