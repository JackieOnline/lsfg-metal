// metal memscan (LSFGM_MEMSCAN): read-only engine memory reconnaissance.
//
// The yysls.exe image is packed (raw .text/.rdata size zero) and unpacks at
// runtime - but our shim already lives in the same process, so the unpacked
// engine is just memory we can read. This module walks the whole address map
// through mach_vm_region and reads chunks through mach_vm_read_overwrite (the
// kernel-mediated copy: a vanished page returns an error code, it cannot
// SIGSEGV the game), pattern-matching for the velocity/cvar strings that
// static analysis could never see through the packer.
//
// Budgeted like the recon probes: SCAN_BUDGET bytes per present tick, so the
// walk spreads over ~30-60s of normal play instead of stalling one frame.
// Nothing is ever written into engine memory - this is observation only.
//
// Targets: the cvar registry (EnableAMDFSR2), the FSR2 pass family
// (ReprojectMotionVector / ReconstructDilate), the generic motion-vector
// machinery, and the frame-generation markers. Regions with hits are dumped
// to LSFGM_MEMSCAN_DIR (default ~/lsfg-metal/memdump) for offline analysis;
// the largest RX region (the unpacked engine .text) is always dumped whole
// for Ghidra. Hits inside our own module are excluded via dladdr.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::log;

const PATTERNS: [&[u8]; 8] = [
    b"EnableAMDFSR2",
    b"ReprojectMotionVector",
    b"ReconstructDilate",
    b"MotionVector",
    b"ScreenUpscaleMethod",
    b"EnableDLSSG",
    b"NVSDK_NGX",
    b"VelocityBuffer",
];
const HITS_PER_PATTERN: usize = 256;

const SCAN_BUDGET: u64 = 16 << 20; // bytes read+matched per tick
const DUMP_BUDGET: u64 = 16 << 20; // bytes written per tick
const CHUNK: u64 = 4 << 20; // mach_vm_read_overwrite granularity
const REGION_CAP: u64 = 256 << 20; // skip arena-scale mappings; engine images stay in
const HIT_CTX_BEFORE: u64 = 16 << 10; // context window around each unique hit
const HIT_CTX_AFTER: u64 = 112 << 10;
const MAX_UNIQUE_HITS: usize = 1024;
const DUMP_REGION_CAP: u64 = 64 << 20;
const TEXT_DUMP_CAP: u64 = 128 << 20;
const DELAY_TICKS: u64 = 1200; // ~10-20s after launch, engine unpacked by then

// ---- mach FFI (libsystem, no new crate) ----
// NOTE: user_tag is `unsigned long` = 8 bytes on LP64 - the struct is 48
// bytes and VM_REGION_BASIC_INFO_COUNT_64 is 12. A 40-byte layout lets the
// kernel write past the buffer and corrupts the caller's stack.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct RegionInfo64 {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    shared: i32,
    reserved: i32,
    offset: u64,
    behavior: u32,
    user_tag: u64,
}
const REGION_INFO_COUNT: u32 =
    (std::mem::size_of::<RegionInfo64>() / std::mem::size_of::<u32>()) as u32;
extern "C" {
    static mut mach_task_self_: u32;
    fn mach_vm_region(
        target: u32,
        address: *mut u64,
        size: *mut u64,
        flavor: u32,
        info: *mut u8,
        info_cnt: *mut u32,
        object_name: *mut u32,
    ) -> i32;
    fn mach_vm_read_overwrite(
        target: u32,
        address: u64,
        size: u64,
        data: *mut u8,
        outsize: *mut u64,
    ) -> i32;
}
#[repr(C)]
struct DlInfo {
    fname: *const i8,
    fbase: *mut core::ffi::c_void,
    sname: *const i8,
    saddr: *mut core::ffi::c_void,
}
extern "C" {
    fn dladdr(addr: *const core::ffi::c_void, info: *mut DlInfo) -> i32;
}

#[inline(never)]
fn anchor() {}

fn our_module_window() -> (u64, u64) {
    let mut di = DlInfo {
        fname: core::ptr::null(),
        fbase: core::ptr::null_mut(),
        sname: core::ptr::null(),
        saddr: core::ptr::null_mut(),
    };
    let ok = unsafe { dladdr(anchor as *const core::ffi::c_void, &mut di) };
    if ok == 0 || di.fbase.is_null() {
        return (0, 0); // no window, accept self-hits (harmless, logged once)
    }
    (di.fbase as u64, 64 << 20)
}

struct State {
    next_addr: u64,
    resume: Option<(u64, u64, u64)>, // (region addr, off, region size)
    scan_ticks: u64,
    hits: Vec<(u8, u64)>,
    seen: std::collections::HashSet<u64>,
    hits_per_pattern: [usize; PATTERNS.len()],
    text_region: (u64, u64),
    text_queued: bool,
    dumps: Vec<(u64, u64)>, // (addr, len) queue
    walk_done: bool,
    done: bool,
    dumped: u64,
}
impl State {
    fn new() -> State {
        State {
            next_addr: 0x1000,
            resume: None,
            scan_ticks: 0,
            hits: Vec::new(),
            seen: std::collections::HashSet::new(),
            hits_per_pattern: [0; PATTERNS.len()],
            text_region: (0, 0),
            text_queued: false,
            dumps: Vec::new(),
            walk_done: false,
            done: false,
            dumped: 0,
        }
    }
}

const SCAN_TICK_CAP: u64 = 8000; // hard stop: force walk_done + dump what we have

static TICKS: AtomicU64 = AtomicU64::new(0);
static STATE: LazyLock<Mutex<Option<State>>> = LazyLock::new(|| Mutex::new(None));
static MODULE_WINDOW: LazyLock<(u64, u64)> = LazyLock::new(our_module_window);

pub(crate) fn on() -> bool {
    std::env::var_os("LSFGM_MEMSCAN").is_some_and(|v| !v.is_empty())
}

fn dump_dir() -> std::path::PathBuf {
    std::env::var_os("LSFGM_MEMSCAN_DIR")
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
            let mut p = std::path::PathBuf::from(home);
            p.push("lsfg-metal");
            p.push("memdump");
            p
        })
}

fn task() -> u32 {
    unsafe { core::ptr::read(core::ptr::addr_of!(mach_task_self_)) }
}

fn region_at(addr: u64) -> Option<(u64, u64, RegionInfo64)> {
    let mut a = addr;
    let mut size = 0u64;
    let mut info = RegionInfo64::default();
    let mut cnt: u32 = REGION_INFO_COUNT;
    let mut obj: u32 = 0;
    let kr = unsafe {
        mach_vm_region(
            task(),
            &mut a,
            &mut size,
            9, // VM_REGION_BASIC_INFO_64
            &mut info as *mut RegionInfo64 as *mut u8,
            &mut cnt,
            &mut obj,
        )
    };
    if kr != 0 {
        return None;
    }
    Some((a, size, info))
}

fn read_chunk(addr: u64, want: u64, buf: &mut Vec<u8>) -> Option<u64> {
    let want = want.min(buf.len() as u64).max(1);
    let mut got = 0u64;
    let kr = unsafe {
        mach_vm_read_overwrite(
            task(),
            addr,
            want,
            buf.as_mut_ptr(),
            &mut got,
        )
    };
    if kr != 0 {
        return None;
    }
    Some(got)
}

// single pass, first-byte dispatch (six distinct heads), caps per pattern
fn eq_at(bytes: &[u8], i: usize, pat: &[u8]) -> bool {
    pat.len() <= bytes.len() - i && &bytes[i..i + pat.len()] == pat
}
fn record(st: &mut State, pi: usize, addr: u64) {
    if st.hits_per_pattern[pi] < HITS_PER_PATTERN && st.seen.insert(addr) && st.hits.len() < MAX_UNIQUE_HITS {
        st.hits.push((pi as u8, addr));
        st.hits_per_pattern[pi] += 1;
    }
}
fn scan_chunk(bytes: &[u8], base: u64, st: &mut State) {
    let n = bytes.len();
    let mut i = 0usize;
    while i < n {
        match bytes[i] {
            b'E' => {
                if st.hits_per_pattern[0] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[0]) {
                    record(st, 0, base + i as u64);
                    i += PATTERNS[0].len();
                    continue;
                }
                if st.hits_per_pattern[5] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[5]) {
                    record(st, 5, base + i as u64);
                    i += PATTERNS[5].len();
                    continue;
                }
            }
            b'R' => {
                if st.hits_per_pattern[1] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[1]) {
                    record(st, 1, base + i as u64);
                    i += PATTERNS[1].len();
                    continue;
                }
                if st.hits_per_pattern[2] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[2]) {
                    record(st, 2, base + i as u64);
                    i += PATTERNS[2].len();
                    continue;
                }
            }
            b'M' => {
                if st.hits_per_pattern[3] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[3]) {
                    record(st, 3, base + i as u64);
                    i += PATTERNS[3].len();
                    continue;
                }
            }
            b'S' => {
                if st.hits_per_pattern[4] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[4]) {
                    record(st, 4, base + i as u64);
                    i += PATTERNS[4].len();
                    continue;
                }
            }
            b'N' => {
                if st.hits_per_pattern[6] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[6]) {
                    record(st, 6, base + i as u64);
                    i += PATTERNS[6].len();
                    continue;
                }
            }
            b'V' => {
                if st.hits_per_pattern[7] < HITS_PER_PATTERN && eq_at(bytes, i, PATTERNS[7]) {
                    record(st, 7, base + i as u64);
                    i += PATTERNS[7].len();
                    continue;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

// dump a context window around every hit inside this region (clipped to
// the region bounds - a sized read can never cross the region end)
fn queue_region(st: &mut State, raddr: u64, rsize: u64) {
    let rend = raddr + rsize;
    for &(_, ha) in st.hits.iter() {
        if ha < raddr || ha >= rend {
            continue;
        }
        let start = ha.saturating_sub(HIT_CTX_BEFORE).max(raddr);
        let end = (ha + HIT_CTX_AFTER).min(rend);
        let (w, l) = (start, end - start);
        if l == 0 {
            continue;
        }
        if !st.dumps.iter().any(|&(a, len)| a <= w && w < a + len) {
            st.dumps.push((w, l));
        }
    }
}

pub(crate) fn tick() {
    if !on() {
        return;
    }
    let t = TICKS.fetch_add(1, Ordering::Relaxed);
    if t < DELAY_TICKS {
        return;
    }
    let mut g = STATE.lock().unwrap();
    if g.is_none() {
        *g = Some(State::new());
        log::info("MEMSCAN armed (read-only walk, budgeted per present)");
    }
    let st = g.as_mut().unwrap();
    if st.done {
        return;
    }

    // ---- phase 1: walk + scan ----
    if !st.walk_done {
        st.scan_ticks += 1;
        if st.scan_ticks > SCAN_TICK_CAP {
            log::info("MEMSCAN walk capped: forcing completion with partial data");
            st.walk_done = true;
        }
        let mut budget = SCAN_BUDGET;
        let mut scratch = vec![0u8; CHUNK as usize];
        while budget > 0 && !st.walk_done {
            // find / resume the region to scan
            let (raddr, roff, rsize, info) = match st.resume {
                Some((a, o, _)) => match region_at(a) {
                    Some((a2, s2, i2)) => (a2, o, s2, i2),
                    None => {
                        st.resume = None;
                        continue;
                    }
                },
                None => match region_at(st.next_addr) {
                    Some((a, s, i)) => {
                        st.next_addr = a.saturating_add(s).max(a + 0x1000);
                        if s == 0 {
                            continue;
                        }
                        (a, 0, s, i)
                    }
                    None => {
                        st.walk_done = true;
                        break;
                    }
                },
            };
            if info.protection & 4 != 0 && info.protection & 1 != 0 && rsize > st.text_region.1 {
                st.text_region = (raddr, rsize);
            }
            let (mbase, mlen) = *MODULE_WINDOW;
            let ours = mbase != 0 && raddr >= mbase && raddr < mbase + mlen;
            if info.protection & 1 == 0 || ours || rsize > REGION_CAP {
                st.resume = None;
                continue;
            }
            // scan this region in chunks under budget
            let mut off = roff;
            let mut region_hit = false;
            while off < rsize && budget > 0 {
                let want = (rsize - off).min(CHUNK);
                match read_chunk(raddr + off, want, &mut scratch) {
                    Some(got) if got > 0 => {
                        let before: u64 = st.hits_per_pattern.iter().map(|&x| x as u64).sum();
                        scan_chunk(&scratch[..got as usize], raddr + off, st);
                        let after: u64 = st.hits_per_pattern.iter().map(|&x| x as u64).sum();
                        region_hit |= after > before;
                        budget = budget.saturating_sub(got);
                        off += got;
                    }
                    _ => {
                        off = rsize; // unreadable tail: give up on this region
                    }
                }
            }
            if region_hit {
                queue_region(st, raddr, rsize);
            }
            if off >= rsize {
                st.resume = None;
            } else {
                st.resume = Some((raddr, off, rsize));
                break; // budget exhausted mid-region
            }
        }
        if st.scan_ticks % 30 == 0 {
            log::log_fmt(
                log::Level::Info,
                format_args!(
                    "MEMSCAN progress t={} next={:#x} hits={} text={:#x}+{}MB",
                    st.scan_ticks,
                    st.next_addr,
                    st.hits.len(),
                    st.text_region.0,
                    st.text_region.1 >> 20
                ),
            );
        }
        if st.walk_done {
            // queue the unpacked engine .text for offline disassembly
            let (a, s) = st.text_region;
            if a != 0 && !st.text_queued {
                st.text_queued = true;
                st.dumps.push((a, s.min(TEXT_DUMP_CAP)));
                log::log_fmt(
                    log::Level::Info,
                    format_args!(
                        "MEMSCAN walk done: {} hits, text region {:#x}+{:#x}MB, {} regions queued",
                        st.hits.len(),
                        a,
                        s >> 20,
                        st.dumps.len()
                    ),
                );
            }
        }
        return;
    }

    // ---- phase 2: drain dumps ----
    let dir = dump_dir();
    let mut wrote = 0u64;
    while wrote < DUMP_BUDGET && !st.dumps.is_empty() {
        let (addr, len) = st.dumps[0];
        let path = dir.join(format!("r{addr:x}_{len:x}.bin"));
        if !dir.exists() {
            let _ = std::fs::create_dir_all(&dir);
        }
        let remaining = len - st.dumped;
        let take = remaining.min(DUMP_BUDGET - wrote);
        let mut scratch = vec![0u8; take as usize];
        let ok = read_chunk(addr + st.dumped, take, &mut scratch);
        if ok.is_none() {
            log::log_fmt(
                log::Level::Info,
                format_args!("MEMSCAN dump unreadable at {:#x}", addr + st.dumped),
            );
            st.dumps.remove(0);
            st.dumped = 0;
            continue;
        }
        let append = st.dumped > 0;
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.create(true);
        if append {
            opts.append(true);
        } else {
            opts.write(true); // .create without write/append cannot create
        }
        let mut f = opts.open(&path);
        match f.as_mut() {
            Ok(f) => match f.write_all(&scratch) {
                Ok(()) => {
                    st.dumped += take;
                    wrote += take;
                    if st.dumped >= len {
                        st.dumps.remove(0);
                        st.dumped = 0;
                    }
                }
                Err(e) => {
                    log::log_fmt(
                        log::Level::Info,
                        format_args!("MEMSCAN dump write err {e}"),
                    );
                    st.dumps.remove(0);
                    st.dumped = 0;
                }
            },
            Err(e) => {
                log::log_fmt(log::Level::Info, format_args!("MEMSCAN dump open err {e}"));
                st.dumps.remove(0);
                st.dumped = 0;
            }
        }
    }
    if st.dumps.is_empty() && !st.done {
        st.done = true;
        // final summary
        let mut lines = String::from("MEMSCAN done:");
        for (pi, n) in st.hits_per_pattern.iter().enumerate() {
            if *n > 0 {
                lines.push_str(&format!(" {} x{},", String::from_utf8_lossy(PATTERNS[pi]), n));
            }
        }
        log::info(&lines);
        for (pi, addr) in st.hits.iter().take(400) {
            log::log_fmt(
                log::Level::Info,
                format_args!(
                    "MEMSCAN hit {} @ {:#x}",
                    String::from_utf8_lossy(PATTERNS[*pi as usize]),
                    addr
                ),
            );
        }
    }
}
