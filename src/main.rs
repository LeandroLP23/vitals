#![windows_subsystem = "windows"]

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::{mem, ptr};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::*;
use windows_sys::Win32::System::Performance::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::*;
use windows_sys::Win32::System::Threading::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Look & feel, measured on a screenshot of the NVIDIA App overlay at 2560x1440.
const TEXT: u32 = rgb(0xEE, 0xEE, 0xEE);
// Pixels of exactly this color become see-through; must differ from TEXT.
const KEY: u32 = rgb(1, 1, 1);
const LABEL_GAP: i32 = 11; // label -> value, px at 96 dpi
const SEP_GAP: i32 = 13; // around "|", px at 96 dpi

// Not redistributable: looked up next to the exe, then in the NVIDIA driver install.
const FONT_FILES: [&str; 2] = ["NVIDIASans_Rg.ttf", "NVIDIASans_Bd.ttf"];
const FONT: &str = "NVIDIA Sans";
const FALLBACK_FONT: &str = "Segoe UI";
const FONT_PX: i32 = 19; // at 96 dpi
const PAD: i32 = 8; // at 96 dpi
const MARGIN: i32 = 15; // distance from the top screen edge, at 96 dpi
const TICK_MS: u32 = 1000;

const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | g << 8 | b << 16
}

fn wz(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

// ---- GPU via Windows performance counters (same source as Task Manager) ----

struct Gpu {
    query: isize,
    eng: isize,
    mem: isize,
    total: u64,
}

impl Gpu {
    fn load() -> Option<Gpu> {
        unsafe {
            let mut p = Gpu {
                query: 0,
                eng: 0,
                mem: 0,
                total: 0,
            };
            if PdhOpenQueryW(ptr::null(), 0, &mut p.query) != 0 {
                return None;
            }
            PdhAddEnglishCounterW(
                p.query,
                wz("\\GPU Engine(*)\\Utilization Percentage").as_ptr(),
                0,
                &mut p.eng,
            );
            PdhAddEnglishCounterW(
                p.query,
                wz("\\GPU Adapter Memory(*)\\Dedicated Usage").as_ptr(),
                0,
                &mut p.mem,
            );
            PdhCollectQueryData(p.query);
            // Dedicated VRAM size per display adapter, as the driver reports it to Windows.
            for i in 0..10 {
                let key = wz(&format!("SYSTEM\\CurrentControlSet\\Control\\Class\\{{4d36e968-e325-11ce-bfc1-08002be10318}}\\{i:04}"));
                let (mut v, mut sz) = (0u64, 8u32);
                if RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    key.as_ptr(),
                    wz("HardwareInformation.qwMemorySize").as_ptr(),
                    RRF_RT_ANY,
                    ptr::null_mut(),
                    &mut v as *mut u64 as *mut c_void,
                    &mut sz,
                ) == 0
                {
                    p.total = p.total.max(v);
                }
            }
            Some(p)
        }
    }

    unsafe fn each(c: isize, mut f: impl FnMut(String, f64)) {
        let (mut size, mut n) = (0u32, 0u32);
        if PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut n, ptr::null_mut())
            != PDH_MORE_DATA
        {
            return;
        }
        let mut buf = vec![0u64; (size as usize).div_ceil(8)];
        if PdhGetFormattedCounterArrayW(c, PDH_FMT_DOUBLE, &mut size, &mut n, buf.as_mut_ptr() as _)
            != 0
        {
            return;
        }
        for it in std::slice::from_raw_parts(
            buf.as_ptr() as *const PDH_FMT_COUNTERVALUE_ITEM_W,
            n as usize,
        ) {
            if it.FmtValue.CStatus <= PDH_CSTATUS_NEW_DATA {
                let len = (0..).take_while(|&i| *it.szName.add(i) != 0).count();
                f(
                    String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, len)),
                    it.FmtValue.Anonymous.doubleValue,
                );
            }
        }
    }

    /// (GPU %, VRAM %). GPU % = busiest engine, summed over processes, like Task Manager.
    fn read(&self) -> (Option<f64>, Option<f64>) {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return (None, None);
            }
            // Instance: pid_<pid>_luid_<..>_phys_<n>_eng_<n>_engtype_<type>
            let mut engines = std::collections::HashMap::new();
            Self::each(self.eng, |name, v| {
                *engines
                    .entry(
                        name.split_once("_luid")
                            .map_or(name.clone(), |x| x.1.to_owned()),
                    )
                    .or_insert(0.0) += v;
            });
            let mut used = 0.0f64;
            Self::each(self.mem, |_, v| used = used.max(v));
            let gpu = engines
                .values()
                .cloned()
                .reduce(f64::max)
                .map(|v| v.min(100.0));
            (
                gpu.or(Some(0.0)),
                (self.total > 0).then(|| used * 100.0 / self.total as f64),
            )
        }
    }
}

// ---- CPU / RAM ----

fn ft(f: FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

fn cpu_times() -> (u64, u64) {
    let (mut i, mut k, mut u) = unsafe { (mem::zeroed(), mem::zeroed(), mem::zeroed()) };
    unsafe { GetSystemTimes(&mut i, &mut k, &mut u) };
    (ft(i), ft(k) + ft(u)) // kernel time includes idle
}

fn ram_pct() -> f64 {
    let mut m: MEMORYSTATUSEX = unsafe { mem::zeroed() };
    m.dwLength = mem::size_of::<MEMORYSTATUSEX>() as u32;
    unsafe { GlobalMemoryStatusEx(&mut m) };
    m.dwMemoryLoad as f64
}

// ---- Window ----

struct Ui {
    fonts: [HFONT; 2], // regular, bold
    pad: i32,
    margin: i32,
    label_gap: i32,
    sep_gap: i32,
    segs: Vec<(Vec<u16>, bool, i32)>, // (text, bold, gap after)
    cpu_prev: (u64, u64),
    gpu: Option<Gpu>,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn fmt(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.0} %"))
}

unsafe fn extent(dc: HDC, ui: &Ui, s: &[u16], bold: bool) -> SIZE {
    SelectObject(dc, ui.fonts[bold as usize]);
    let mut sz: SIZE = mem::zeroed();
    GetTextExtentPoint32W(dc, s.as_ptr(), s.len() as i32, &mut sz);
    sz
}

unsafe fn tick(h: HWND) {
    if IsWindowVisible(h) == 0 {
        return;
    }
    let pos = UI.with_borrow_mut(|ui| {
        let ui = ui.as_mut().unwrap();
        let (idle, total) = cpu_times();
        let (pi, pt) = mem::replace(&mut ui.cpu_prev, (idle, total));
        let cpu = (total > pt).then(|| 100.0 * (1.0 - (idle - pi) as f64 / (total - pt) as f64));
        let (gpu, vram) = ui.gpu.as_ref().map_or((None, None), Gpu::read);

        let metrics = [
            ("CPU", cpu),
            ("RAM", Some(ram_pct())),
            ("GPU", gpu),
            ("VRAM", vram),
        ];
        ui.segs.clear();
        for (i, (label, val)) in metrics.into_iter().enumerate() {
            if i > 0 {
                ui.segs
                    .push(("|".encode_utf16().collect(), false, ui.sep_gap));
            }
            ui.segs
                .push((label.encode_utf16().collect(), false, ui.label_gap));
            ui.segs
                .push((fmt(val).encode_utf16().collect(), true, ui.sep_gap));
        }
        ui.segs.last_mut().unwrap().2 = 0;

        let dc = GetDC(h);
        let (tw, th) = ui
            .segs
            .iter()
            .map(|(s, b, g)| (extent(dc, ui, s, *b), *g))
            .fold((0, 0), |(w, h), (sz, g)| (w + sz.cx + g, h.max(sz.cy)));
        ReleaseDC(h, dc);
        let (w, hh) = (tw + 2 * ui.pad, th + ui.pad);
        ((GetSystemMetrics(SM_CXSCREEN) - w) / 2, ui.margin, w, hh)
    });
    // Re-assert topmost every tick: fullscreen-ish apps keep pushing themselves on top.
    SetWindowPos(h, HWND_TOPMOST, pos.0, pos.1, pos.2, pos.3, SWP_NOACTIVATE);
    InvalidateRect(h, ptr::null(), 0);
}

unsafe fn paint(h: HWND) {
    let mut ps: PAINTSTRUCT = mem::zeroed();
    let dc = BeginPaint(h, &mut ps);
    let mut rc: RECT = mem::zeroed();
    GetClientRect(h, &mut rc);
    let br = CreateSolidBrush(KEY);
    FillRect(dc, &rc, br);
    DeleteObject(br);
    UI.with_borrow(|ui| {
        let ui = ui.as_ref().unwrap();
        SetBkMode(dc, TRANSPARENT as _);
        SetTextColor(dc, TEXT);
        let mut x = ui.pad;
        for (s, bold, gap) in &ui.segs {
            let w = extent(dc, ui, s, *bold).cx; // also selects the font
            TextOutW(dc, x, ui.pad / 2, s.as_ptr(), s.len() as i32);
            x += w + gap;
        }
    });
    EndPaint(h, &ps);
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => tick(h),
        WM_PAINT => paint(h),
        WM_HOTKEY => {
            if IsWindowVisible(h) != 0 {
                ShowWindow(h, SW_HIDE);
            } else {
                ShowWindow(h, SW_SHOWNOACTIVATE);
                tick(h);
            }
        }
        WM_DESTROY => PostQuitMessage(0),
        _ => return DefWindowProcW(h, msg, wp, lp),
    }
    0
}

fn font_dir() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let installer =
        PathBuf::from(std::env::var_os("ProgramFiles")?).join(r"NVIDIA Corporation\Installer2");
    let driver_dirs = std::fs::read_dir(installer)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path());
    std::iter::once(exe_dir)
        .chain(driver_dirs)
        .find(|d| FONT_FILES.iter().all(|f| d.join(f).is_file()))
}

fn main() {
    unsafe {
        CreateMutexW(ptr::null(), 0, wz("Local\\Vitals").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        SetProcessDPIAware();

        let screen = GetDC(ptr::null_mut());
        let dpi = GetDeviceCaps(screen, LOGPIXELSY as _);
        ReleaseDC(ptr::null_mut(), screen);
        let px = |v: i32| v * dpi / 96;
        let loaded = font_dir().is_some_and(|dir| {
            FONT_FILES.iter().all(|f| {
                AddFontResourceExW(
                    wz(&dir.join(f).to_string_lossy()).as_ptr(),
                    FR_PRIVATE,
                    ptr::null(),
                ) > 0
            })
        });
        let face = wz(if loaded { FONT } else { FALLBACK_FONT });
        let mk = |weight: u32| {
            CreateFontW(
                -px(FONT_PX),
                0,
                0,
                0,
                weight as _,
                0,
                0,
                0,
                DEFAULT_CHARSET as _,
                OUT_DEFAULT_PRECIS as _,
                CLIP_DEFAULT_PRECIS as _,
                ANTIALIASED_QUALITY as _,
                DEFAULT_PITCH as _,
                face.as_ptr(),
            )
        };
        UI.set(Some(Ui {
            fonts: [mk(FW_NORMAL as _), mk(FW_BOLD as _)],
            pad: px(PAD),
            margin: px(MARGIN),
            label_gap: px(LABEL_GAP),
            sep_gap: px(SEP_GAP),
            segs: Vec::new(),
            cpu_prev: cpu_times(),
            gpu: Gpu::load(),
        }));

        let hinst = GetModuleHandleW(ptr::null());
        let class = wz("Vitals");
        // Task Manager shows the window's icon, not the exe's. Resource 1 is in assets/vitals.rc.
        let icon = |size| {
            let px = GetSystemMetrics(size);
            LoadImageW(hinst, 1 as _, IMAGE_ICON, px, px, LR_DEFAULTCOLOR) as HICON
        };
        let wc = WNDCLASSEXW {
            cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: icon(SM_CXICON),
            hIconSm: icon(SM_CXSMICON),
            lpszClassName: class.as_ptr(),
            ..mem::zeroed()
        };
        RegisterClassExW(&wc);
        let h = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            ptr::null_mut(),
            ptr::null_mut(),
            hinst,
            ptr::null(),
        );
        SetLayeredWindowAttributes(h, KEY, 0, LWA_COLORKEY);
        SetTimer(h, 1, TICK_MS, None);

        let hotkey = RegisterHotKey(h, 1, MOD_ALT | MOD_NOREPEAT, 'R' as u32) != 0;
        if !hotkey {
            MessageBoxW(h, wz("Alt+R e' gia' usato da un altro programma (overlay NVIDIA?).\nL'overlay restera' sempre visibile.").as_ptr(),
                class.as_ptr(), MB_OK | MB_ICONWARNING);
        }
        if !hotkey || std::env::args().any(|a| a == "--show") {
            ShowWindow(h, SW_SHOWNOACTIVATE);
            tick(h);
        }

        let mut msg: MSG = mem::zeroed();
        while GetMessageW(&mut msg, ptr::null_mut(), 0, 0) > 0 {
            DispatchMessageW(&msg);
        }
    }
}
