// metal recon (LSFGM_RECON): texture census + velocity sweep with triage
//
// The game renders D3D11 -> Metal directly (DXMT); the Vulkan shim path is
// dormant for it. This module swizzles the concrete MTLDevice class's
// newTextureWithDescriptor: to log every texture, holds references to the
// motion-candidate family, and sweeps them through the worker's own dump
// machinery (shared buffer + blit + waitUntilCompleted - the context that
// has been dumping source frames via LSFGM_METAL_DUMP forever, never
// getBytes and never the game's threads).
//
// Candidate family, after the recon9/10 sweeps eliminated the RG16Float
// set (dead allocations and lookup tables): RG8Unorm at the render and
// output resolutions plus its half-res pyramid, RG16Float, RG11B10Float.
// Triage is byte-level and format independent - a written motion field's
// bytes move between standing and panning, lookup tables repeat to four
// decimals, dead ones stay zero. Live candidates write bins at the last
// sample for offline decoding.
//
// History: recon3 constructor install (boot wedged), recon4 view/render
// hooks (crash), recon5 census (stable), recon6/7 getBytes (deadlock),
// recon8 probe getBytes (frame graph jam), recon9/10 worker-queue blits
// (safe; RG16Float eliminated).
//
// Hooks install only when LSFGM_RECON is set; a normal launch is untouched.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Instant;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Imp, ProtocolObject, Sel};
use objc2::{msg_send, sel};
use objc2_metal::{
    MTLBuffer, MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLResourceOptions, MTLSize, MTLOrigin, MTLTexture,
    MTLPixelFormat,
};

use crate::log;

pub(crate) fn recon_on() -> bool {
    std::env::var_os("LSFGM_RECON").is_some()
}

#[derive(Clone)]
struct TexInfo {
    fmt: u64,
    w: u64,
    h: u64,
}

// texture pointer -> descriptor info
static TEX: LazyLock<Mutex<HashMap<usize, TexInfo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
// motion-candidate textures held for the sweep; the Retained keeps them
// alive so the later reads can never dangle
static HELD: LazyLock<Mutex<Vec<HeldTex>>> = LazyLock::new(|| Mutex::new(Vec::new()));
// per-texture statistics history, one (nonzero, mean, max) triple per sample
static VELOCITY_STATS: LazyLock<Mutex<HashMap<usize, Vec<(f32, f32, f32)>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FRAMES: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);
static SAMPLE_T0: OnceLock<Instant> = OnceLock::new();
static FIRST_TEXTURE: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct SendTex(Retained<AnyObject>);
// MTLTexture is safe to reference from other threads: Metal device-side
// objects are thread-safe, and the readback blocks until the GPU is done
unsafe impl Send for SendTex {}

#[derive(Clone)]
struct HeldTex {
    tex: SendTex,
    w: u64,
    h: u64,
    fmt: u64,
}

type NewTextureFn =
    unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> *mut AnyObject;

static ORIG_NEW_TEXTURE: OnceLock<NewTextureFn> = OnceLock::new();

fn fmt_name(f: u64) -> String {
    let known = [
        (MTLPixelFormat::RG16Float, "RG16Float"),
        (MTLPixelFormat::RG16Snorm, "RG16Snorm"),
        (MTLPixelFormat::RG16Unorm, "RG16Unorm"),
        (MTLPixelFormat::RG8Unorm, "RG8Unorm"),
        (MTLPixelFormat::RG11B10Float, "RG11B10Float"),
        (MTLPixelFormat::R16Float, "R16Float"),
        (MTLPixelFormat::R32Float, "R32Float"),
        (MTLPixelFormat::RG32Float, "RG32Float"),
        (MTLPixelFormat::RGBA16Float, "RGBA16Float"),
        (MTLPixelFormat::RGBA32Float, "RGBA32Float"),
        (MTLPixelFormat::Depth32Float, "Depth32Float"),
        (MTLPixelFormat::Depth16Unorm, "Depth16Unorm"),
        (MTLPixelFormat::Depth24Unorm_Stencil8, "Depth24Unorm_Stencil8"),
        (MTLPixelFormat::Depth32Float_Stencil8, "Depth32Float_Stencil8"),
        (MTLPixelFormat::BGRA8Unorm, "BGRA8Unorm"),
        (MTLPixelFormat::RGBA8Unorm, "RGBA8Unorm"),
        (MTLPixelFormat::RGB10A2Unorm, "RGB10A2Unorm"),
    ];
    for (k, n) in known {
        if f == k.0 as u64 {
            return n.into();
        }
    }
    format!("#{f}")
}

// bytes per pixel for the formats the sweep can hold
fn held_bpp(fmt: u64) -> usize {
    if fmt == MTLPixelFormat::RG8Unorm.0 as u64 {
        2
    } else if fmt == MTLPixelFormat::RG32Float.0 as u64 {
        8
    } else {
        // RG16Float and RG11B10Float
        4
    }
}

fn ptr(p: *mut AnyObject) -> String {
    if p.is_null() {
        "null".into()
    } else {
        format!("{:x}", p as usize)
    }
}

fn dump_dir() -> String {
    std::env::var("LSFGM_RECON_DIR").unwrap_or_else(|_| {
        std::env::var("HOME").unwrap_or_else(|_| ".".into()) + "/lsfg-metal"
    })
}

// deferred from the first game-sized layer (see hooks.rs): constructor-time
// Metal device creation in every wine child wedged the boot chain in recon3,
// so nothing here runs until a wide layer presents
pub(crate) fn install_deferred(game_layer: bool) {
    if !game_layer || INSTALLED.swap(true, Ordering::Relaxed) {
        return;
    }
    let Some(dev) = MTLCreateSystemDefaultDevice() else {
        return;
    };
    let cls = unsafe { &*(Retained::as_ptr(&dev) as *const AnyObject) }.class();
    if let Some(m) = cls.instance_method(sel!(newTextureWithDescriptor:)) {
        // publish the original before swapping: a racing creator must find it
        let _ = ORIG_NEW_TEXTURE.set(unsafe {
            std::mem::transmute::<Imp, NewTextureFn>(m.implementation())
        });
        unsafe {
            m.set_implementation(std::mem::transmute::<*const (), Imp>(
                new_texture_hook as *const (),
            ))
        };
    }
    log::info(&format!(
        "RECON metal texture census installed on device {} (pid {})",
        cls.name().to_string_lossy(),
        std::process::id()
    ));
}

unsafe extern "C-unwind" fn new_texture_hook(
    this: *mut AnyObject,
    cmd: Sel,
    desc: *mut AnyObject,
) -> *mut AnyObject {
    let r = (ORIG_NEW_TEXTURE.get().unwrap())(this, cmd, desc);
    if !desc.is_null() {
        recon_log_texture(r, desc);
    }
    r
}

unsafe fn recon_log_texture(tex: *mut AnyObject, desc: *mut AnyObject) {
    let d = &*desc;
    let fmt: u64 = msg_send![d, pixelFormat];
    let w: u64 = msg_send![d, width];
    let h: u64 = msg_send![d, height];
    let mips: u64 = msg_send![d, mipmapLevelCount];
    let ty: u64 = msg_send![d, textureType];
    let usage: u64 = msg_send![d, usage];
    let samples: u64 = msg_send![d, sampleCount];
    if !FIRST_TEXTURE.swap(true, Ordering::Relaxed) {
        log::warn(&format!(
            "RECON first metal texture intercepted, census starts (pid {})",
            std::process::id()
        ));
    }
    if !tex.is_null() {
        TEX.lock().unwrap().insert(tex as usize, TexInfo { fmt, w, h });
        // hold the motion-candidate family: after the recon10 sweep killed
        // the RG16Float set (dead allocations and lookup tables) the live
        // candidates are expected among RG8Unorm's render/output/half-res
        // pyramid, with RG16Float and RG11B10Float kept for completeness
        let candidate = fmt == MTLPixelFormat::RG8Unorm.0 as u64
            || fmt == MTLPixelFormat::RG16Float.0 as u64
            || fmt == MTLPixelFormat::RG11B10Float.0 as u64;
        if candidate && w >= 800 && h >= 400 && mips == 1 {
            let mut held = HELD.lock().unwrap();
            if held.len() < 20 {
                if let Some(r) = Retained::retain(tex) {
                    held.push(HeldTex {
                        tex: SendTex(r),
                        w,
                        h,
                        fmt,
                    });
                    log::info(&format!(
                        "RECON holding {} {}x{} mips=1 {} (#{}) (pid {})",
                        fmt_name(fmt),
                        w,
                        h,
                        ptr(tex),
                        held.len(),
                        std::process::id()
                    ));
                }
            }
        }
    }
    log::info(&format!(
        "RECON texture {} fmt={} {}x{} type={} mips={} usage={} samples={} (pid {})",
        ptr(tex),
        fmt_name(fmt),
        w,
        h,
        ty,
        mips,
        usage,
        samples,
        std::process::id()
    ));
}

// byte-level statistics, format independent: liveness does not need value
// decoding, and the raw bins of live candidates are decoded offline anyway
struct ByteStats {
    nonzero: f32,
    mean_abs: f32,
    max_abs: f32,
}

fn byte_stats(buf: &[u8]) -> ByteStats {
    let step = 16; // four pixels of the widest candidate format
    let mut n = 0u64;
    let mut nonzero = 0u64;
    let mut sum = 0f64;
    let mut max = 0f32;
    let mut i = 0usize;
    while i < buf.len() {
        let v = buf[i] as f32;
        n += 1;
        if v > 0.0 {
            nonzero += 1;
        }
        sum += v as f64;
        if v > max {
            max = v;
        }
        i += step;
    }
    ByteStats {
        nonzero: if n > 0 { nonzero as f32 / n as f32 } else { 0.0 },
        mean_abs: if n > 0 { sum / n as f64 } else { 0.0 } as f32,
        max_abs: max,
    }
}

// called from the worker's process loop, on the worker thread: this is the
// same context the source-frame dumps (LSFGM_METAL_DUMP) have always used -
// a shared-storage buffer, a blit on the worker's own queue and
// waitUntilCompleted, never getBytes and never the game's threads
pub(crate) fn velocity_tick(
    device: &ProtocolObject<dyn MTLDevice>,
    queue: &ProtocolObject<dyn MTLCommandQueue>,
) {
    let held: Vec<HeldTex> = match HELD.lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if held.is_empty() {
        return;
    }
    let t0 = *SAMPLE_T0.get_or_init(Instant::now);
    // sample 1 lands ~10s after the world renderer init (the player stands
    // still), samples 2 and 3 ~25s and ~45s in (the panning window)
    const WINDOWS_SECS: [u64; 3] = [10, 25, 45];
    let done = SAMPLES.load(Ordering::Relaxed);
    if done >= WINDOWS_SECS.len() as u64 {
        return;
    }
    let t = t0.elapsed().as_secs();
    if t < WINDOWS_SECS[done as usize] {
        return;
    }
    let sample = SAMPLES.fetch_add(1, Ordering::Relaxed) + 1;
    let last = sample == WINDOWS_SECS.len() as u64;
    for tex in &held {
        let key = Retained::as_ptr(&tex.tex.0) as usize;
        // on the last sample keep the bin only for candidates whose stats
        // moved: a written motion field varies with the camera, lookup
        // tables repeat to four decimals, dead ones stay zero
        let keep = last
            && match VELOCITY_STATS.lock() {
                Ok(st) => st.get(&key).is_some_and(|v| {
                    if v.len() < 2 {
                        return false;
                    }
                    let (n1, m1, _) = v[0];
                    let (n2, m2, _) = v[v.len() - 1];
                    (n1 - n2).abs() > 2.0 || (m1 - m2).abs() > 0.2 * m1.max(m2).max(0.0001)
                }),
                Err(_) => false,
            };
        let s = dump_velocity_texture(device, queue, tex, sample, t, keep);
        if let Ok(mut st) = VELOCITY_STATS.lock() {
            st.entry(key)
                .or_default()
                .push((s.nonzero * 100.0, s.mean_abs, s.max_abs));
        }
    }
    if last {
        if let Ok(st) = VELOCITY_STATS.lock() {
            for tex in &held {
                let key = Retained::as_ptr(&tex.tex.0) as usize;
                let Some(v) = st.get(&key) else { continue };
                if v.len() < 3 {
                    continue;
                }
                let dead = v.iter().all(|(n, m, _)| *n < 0.01 && *m < 0.01);
                let varies = (v[0].0 - v[2].0).abs() > 2.0
                    || (v[0].1 - v[2].1).abs() > 0.2 * v[0].1.max(v[2].1).max(0.0001);
                let verdict = if dead {
                    "dead"
                } else if varies {
                    "live"
                } else {
                    "static"
                };
                log::info(&format!(
                    "RECON verdict {key:x} {} {}x{} {} nonzero=[{:.2},{:.2},{:.2}]% mean=[{:.3},{:.3},{:.3}] (pid {})",
                    fmt_name(tex.fmt),
                    tex.w,
                    tex.h,
                    verdict,
                    v[0].0,
                    v[1].0,
                    v[2].0,
                    v[0].1,
                    v[1].1,
                    v[2].1,
                    std::process::id()
                ));
            }
        }
        log::info(&format!(
            "RECON velocity sweep complete at t={t}s (pid {})",
            std::process::id()
        ));
    } else {
        log::info(&format!(
            "RECON velocity sample {sample} complete at t={t}s (pid {})",
            std::process::id()
        ));
    }
}

fn dump_velocity_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    queue: &ProtocolObject<dyn MTLCommandQueue>,
    held: &HeldTex,
    sample: u64,
    t: u64,
    keep_bin: bool,
) -> ByteStats {
    let bpp = held_bpp(held.fmt);
    let row = (held.w as usize) * bpp;
    let bytes = row * held.h as usize;
    // the object came out of newTextureWithDescriptor:, so it is an
    // MTLTexture by construction; the codebase casts raw pointers the same
    // way in its drawable hooks
    let texture: &ProtocolObject<dyn MTLTexture> =
        unsafe { &*(Retained::as_ptr(&held.tex.0) as *const ProtocolObject<dyn MTLTexture>) };
    let (Some(buffer), Some(cb)) = (
        device.newBufferWithLength_options(bytes, MTLResourceOptions::StorageModeShared),
        queue.commandBuffer(),
    ) else {
        log::warn("RECON velocity dump: no buffer or command buffer");
        return ByteStats {
            nonzero: 0.0,
            mean_abs: 0.0,
            max_abs: 0.0,
        };
    };
    let Some(blit) = cb.blitCommandEncoder() else {
        log::warn("RECON velocity dump: no blit encoder");
        return ByteStats {
            nonzero: 0.0,
            mean_abs: 0.0,
            max_abs: 0.0,
        };
    };
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture,
            0,
            0,
            MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize { width: held.w as usize, height: held.h as usize, depth: 1 },
            &buffer,
            0,
            row,
            bytes,
        );
    }
    blit.endEncoding();
    cb.commit();
    cb.waitUntilCompleted();
    let px =
        unsafe { std::slice::from_raw_parts(buffer.contents().as_ptr() as *const u8, bytes) };
    let s = byte_stats(px);
    log::info(&format!(
        "RECON velocity sample={sample} t={t}s {} {}x{} bytes-nonzero={:.4}% mean={:.3} max={:.0} (pid {})",
        fmt_name(held.fmt),
        held.w,
        held.h,
        s.nonzero * 100.0,
        s.mean_abs,
        s.max_abs,
        std::process::id()
    ));
    if keep_bin {
        let path = format!(
            "{}/recon-velocity-s{}-{}-{}x{}.bin",
            dump_dir(),
            sample,
            fmt_name(held.fmt),
            held.w,
            held.h
        );
        match std::fs::write(&path, px) {
            Ok(()) => log::info(&format!("RECON dump {path}")),
            Err(e) => log::warn(&format!("RECON dump {path} failed: {e}")),
        }
    }
    s
}

// called from the layer's nextDrawable hook, once per frame
pub(crate) fn flush() {
    let n = FRAMES.fetch_add(1, Ordering::Relaxed);
    if n % 600 == 0 {
        log::info(&format!(
            "RECON alive: frame #{n} textures={} (pid {})",
            TEX.lock().unwrap().len(),
            std::process::id()
        ));
    }
}
