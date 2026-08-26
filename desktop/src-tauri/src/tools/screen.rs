//! Looking at the screen: which windows exist, where they are, and what they show.
//!
//! Every command here is a read. None of them moves anything, and none of them needs
//! the session grant — looking is not acting, and requiring a grant to look would mean
//! the agent had to be given permission to click before it could work out whether
//! clicking was necessary. That would be exactly backwards: the discipline this
//! feature depends on is look, act, verify, and the looking has to be free or it will
//! be skipped.
//!
//! WHAT IS STILL REFUSED, EVEN THOUGH THESE ARE READS
//!
//! The exclusion list applies here as strictly as it does to clicking, and for a
//! reason worth stating: a screenshot of a password manager with its vault open is a
//! password leak, and it reaches the model and the provider. A window that may not be
//! clicked may not be photographed either. `pc.list_windows` also omits excluded
//! windows entirely rather than listing them as hidden, because a list that says
//! "1Password (excluded)" has already told the model that he uses 1Password and that
//! it is open.
//!
//! WHY xcap
//!
//! Capture on Windows means either the GDI route (`BitBlt` from a window or screen DC)
//! or the modern Windows.Graphics.Capture API, and both are a few hundred lines of
//! unsafe with edge cases around DPI, multiple monitors and hardware-accelerated
//! surfaces. `xcap` wraps that, is maintained, and already carries the monitor
//! enumeration this module needs. Hand-rolling it would mean writing the same unsafe
//! code worse, next to a security boundary.
//!
//! Window enumeration is hand-rolled against the `windows` crate rather than taken
//! from xcap, because what is needed — title, class, process, rect, focus — is four
//! straightforward calls, and xcap's window type does not expose the class name that
//! the exclusion matcher checks.
//!
//! COORDINATES
//!
//! Everything here reports and accepts *physical* pixels, and says so. A DPI-scaled
//! desktop makes "logical" ambiguous — logical to whom, at what scale factor, on which
//! monitor — and the only coordinates that mean one thing are the ones the OS uses for
//! the virtual screen. The monitor list reports each scale factor so the model can
//! reason about what it is seeing.

use crate::agent::{Agent, ToolOut};
use crate::grant;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, State};

/// The longest `pc.wait` may pause for. A tool that can sleep indefinitely is a tool
/// that can hang a turn, and the wall-clock budget would eventually notice but only
/// after the user had been staring at nothing.
pub const MAX_WAIT_MS: u64 = 10_000;

/// Largest edge of a returned screenshot, before JPEG encoding. Screenshots are mostly
/// small text, so this is generous compared with a photo — an unreadable screenshot is
/// worse than none, because the model will confidently describe it anyway.
const MAX_SHOT_EDGE: u32 = 1600;

/// JPEG quality, and the byte ceiling it is reduced to fit. The worker refuses a
/// single image over 2.4 MB of data URL, so this stays clear of it.
const SHOT_QUALITY: u8 = 82;
const MAX_SHOT_BYTES: usize = 1_400_000;

#[derive(Debug, Deserialize)]
pub struct WaitReq {
    #[serde(default)]
    pub ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct ScreenshotReq {
    /// A window id from `pc.list_windows`. Omit for a whole monitor.
    #[serde(default)]
    pub window: i64,
    /// A monitor index from `pc.list_monitors`. Omit with no window for the primary.
    #[serde(default)]
    pub monitor: i64,
}

#[derive(Debug, Serialize)]
pub struct WindowInfo {
    /// The HWND as an integer. Opaque to the model; it comes from a listing and goes
    /// back unchanged, exactly like a Notion page id.
    pub id: i64,
    pub title: String,
    pub process: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub focused: bool,
    pub minimised: bool,
}

#[derive(Debug, Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub primary: bool,
}

/* ── window enumeration ──────────────────────────────────────────── */

#[cfg(windows)]
mod win {
    use super::WindowInfo;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    };

    struct Collected {
        windows: Vec<(WindowInfo, String)>,
        foreground: isize,
    }

    /// Read a window's class name, which the exclusion matcher needs and which a title
    /// alone cannot substitute for: a password manager may present a generic title on a
    /// distinctive class.
    unsafe fn class_of(hwnd: HWND) -> String {
        let mut buf = [0u16; 256];
        let n = GetClassNameW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }

    unsafe fn title_of(hwnd: HWND) -> String {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len as usize) + 1];
        let n = GetWindowTextW(hwnd, &mut buf);
        if n <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..n as usize])
    }

    /// The executable behind a window, by name only.
    ///
    /// The full path is deliberately discarded: it is not needed to identify an
    /// application, and a path under the user's profile leaks a username into the
    /// model's context for no benefit.
    unsafe fn process_of(hwnd: HWND) -> String {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return String::new();
        }
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if ok.is_err() || len == 0 {
            return String::new();
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
    }

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Collected);

        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }
        let title = title_of(hwnd);
        // A visible window with no title is a tool window, a tray host or a shell
        // artefact. Listing them would bury the handful the user recognises.
        if title.trim().is_empty() {
            return TRUE;
        }

        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return TRUE;
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        if w <= 0 || h <= 0 {
            return TRUE;
        }

        out.windows.push((
            WindowInfo {
                id: hwnd.0 as i64,
                title,
                process: process_of(hwnd),
                x: rect.left,
                y: rect.top,
                width: w,
                height: h,
                focused: hwnd.0 as isize == out.foreground,
                minimised: IsIconic(hwnd).as_bool(),
            },
            class_of(hwnd),
        ));
        TRUE
    }

    /// Every visible top-level window, with its class alongside so the caller can
    /// apply the exclusion list before anything is returned.
    pub fn enumerate() -> Vec<(WindowInfo, String)> {
        let mut collected = Collected {
            windows: Vec::new(),
            foreground: unsafe { GetForegroundWindow().0 as isize },
        };
        unsafe {
            let _ = EnumWindows(
                Some(visit),
                LPARAM(&mut collected as *mut Collected as isize),
            );
        }
        collected.windows
    }

    /// Where the pointer is, in physical virtual-screen pixels.
    pub fn cursor() -> (i32, i32) {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }

    /// The title and class of a window by id, for checking one that was named rather
    /// than listed.
    pub fn describe(id: i64) -> Option<(String, String)> {
        let hwnd = HWND(id as *mut core::ffi::c_void);
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() {
                return None;
            }
            Some((title_of(hwnd), class_of(hwnd)))
        }
    }
}

#[cfg(not(windows))]
mod win {
    use super::WindowInfo;
    pub fn enumerate() -> Vec<(WindowInfo, String)> {
        Vec::new()
    }
    pub fn cursor() -> (i32, i32) {
        (0, 0)
    }
    pub fn describe(_id: i64) -> Option<(String, String)> {
        None
    }
}

/// Visible windows, minus the ones that are never to be touched.
///
/// Excluded windows are omitted rather than listed as hidden. A list containing
/// "1Password (excluded)" has already told the model that he uses 1Password and that
/// it is open, which is information it does not need and should not have.
pub fn visible_windows(extra: &[String]) -> Vec<WindowInfo> {
    win::enumerate()
        .into_iter()
        .filter(|(info, class)| !grant::is_blocked(&info.title, class, extra))
        .map(|(info, _)| info)
        .collect()
}

/// Is this window one the agent may act on? Named separately from the listing because
/// a window can be named by id without having been listed — a stale id from an earlier
/// round, or one the model invented.
pub fn window_allowed(id: i64, extra: &[String]) -> Result<String, String> {
    match win::describe(id) {
        None => Err(
            "there is no visible window with that id any more. List the windows again \u{2014} ids \
             change when windows close."
                .into(),
        ),
        Some((title, class)) => {
            if grant::is_blocked(&title, &class, extra) {
                Err(format!(
                    "Compass will not touch \u{201C}{}\u{201D}. Security and password windows are \
                     off limits, and no wording changes that.",
                    title.chars().take(60).collect::<String>()
                ))
            } else {
                Ok(title)
            }
        }
    }
}

/* ── the commands ────────────────────────────────────────────────── */

#[tauri::command]
pub async fn pc_list_windows(state: State<'_, Agent>) -> Result<ToolOut, String> {
    let pol = state.policy();
    let wins = visible_windows(&pol.blocked_windows);

    let mut text = format!("WINDOWS \u{2014} {} visible:\n", wins.len());
    if wins.is_empty() {
        text.push_str("  (none, which usually means every window is minimised)\n");
    }
    for w in &wins {
        text.push_str(&format!(
            "  id={} {}\u{201C}{}\u{201D} [{}] at {},{} size {}x{}{}\n",
            w.id,
            if w.focused { "*focused* " } else { "" },
            w.title.chars().take(80).collect::<String>(),
            w.process,
            w.x,
            w.y,
            w.width,
            w.height,
            if w.minimised { " (minimised)" } else { "" }
        ));
    }
    text.push_str(
        "\nThe id is what pc.focus_window and pc.screenshot take. Coordinates are physical \
         screen pixels. Security and password windows are never listed.\n",
    );

    state.audit.record(
        "pc.list_windows",
        true,
        format!("Listed {} visible window(s)", wins.len()),
        None,
        false,
    );
    Ok(ToolOut::text(text))
}

#[tauri::command]
pub async fn pc_list_monitors(state: State<'_, Agent>) -> Result<ToolOut, String> {
    let mons = monitors()?;
    let mut text = format!("MONITORS \u{2014} {}:\n", mons.len());
    for m in &mons {
        text.push_str(&format!(
            "  index={} {}x{} at {},{} scale {:.0}%{}\n",
            m.index,
            m.width,
            m.height,
            m.x,
            m.y,
            m.scale * 100.0,
            if m.primary { " (primary)" } else { "" }
        ));
    }
    text.push_str(
        "\nCoordinates are physical pixels on the combined desktop, which is the space every \
         pc.* coordinate is in. A scale above 100% means the desktop is DPI-scaled, so what \
         looks like a 14-point font occupies more pixels than you would expect.\n",
    );
    state.audit.record(
        "pc.list_monitors",
        true,
        format!("Listed {} monitor(s)", mons.len()),
        None,
        false,
    );
    Ok(ToolOut::text(text))
}

pub fn monitors() -> Result<Vec<MonitorInfo>, String> {
    let found = xcap::Monitor::all().map_err(|e| format!("could not read the monitors: {e}"))?;
    let mut out = Vec::new();
    for (i, m) in found.iter().enumerate() {
        out.push(MonitorInfo {
            index: i,
            x: m.x().unwrap_or(0),
            y: m.y().unwrap_or(0),
            width: m.width().unwrap_or(0),
            height: m.height().unwrap_or(0),
            scale: m.scale_factor().unwrap_or(1.0),
            primary: m.is_primary().unwrap_or(false),
        });
    }
    if out.is_empty() {
        return Err("Windows reported no monitors, which should not happen".into());
    }
    Ok(out)
}

#[tauri::command]
pub async fn pc_cursor_position(state: State<'_, Agent>) -> Result<ToolOut, String> {
    let (x, y) = win::cursor();
    state.audit.record(
        "pc.cursor_position",
        true,
        format!("Pointer at {x},{y}"),
        None,
        false,
    );
    Ok(ToolOut::text(format!(
        "POINTER at {x},{y} (physical screen pixels)"
    )))
}

/// Pause, so a window has time to open or a menu to appear.
///
/// Bounded, and the bound is low. Something that can sleep indefinitely can hang a
/// turn, and the wall-clock budget would eventually notice but only after the user had
/// spent a minute watching nothing.
#[tauri::command]
pub async fn pc_wait(state: State<'_, Agent>, req: WaitReq) -> Result<ToolOut, String> {
    let ms = req.ms.clamp(50, MAX_WAIT_MS);
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    state
        .audit
        .record("pc.wait", true, format!("Waited {ms} ms"), None, false);
    Ok(ToolOut::text(format!("Waited {ms} ms.")))
}

/// A picture of the screen, a monitor, or one window.
///
/// Returns a data URL, which goes into the same attachment pipeline a photo does — so
/// the worker's image validation, the size limits and the vision path are all the ones
/// that already existed. There is deliberately no second image path.
#[tauri::command]
pub async fn pc_screenshot(
    app: AppHandle,
    state: State<'_, Agent>,
    req: ScreenshotReq,
) -> Result<ToolOut, String> {
    let _ = &app;
    let pol = state.policy();

    let out = tauri::async_runtime::spawn_blocking({
        let extra = pol.blocked_windows.clone();
        let req_window = req.window;
        let req_monitor = req.monitor;
        move || capture(req_window, req_monitor, &extra)
    })
    .await
    .map_err(|_| "the screen capture failed unexpectedly".to_string())?;

    let (data_url, what, w, h) = match out {
        Ok(v) => v,
        Err(e) => {
            state.audit.record(
                "pc.screenshot",
                false,
                "Screen capture".into(),
                Some(e.clone()),
                false,
            );
            return Err(e);
        }
    };

    state.audit.record(
        "pc.screenshot",
        true,
        format!("Captured {what} ({w}x{h})"),
        None,
        false,
    );

    /* The image travels in the text as a data URL with a marker the frontend looks
    for. That is uglier than a typed field and was chosen anyway: ToolOut is the one
    shape every tool returns, the frontend already has one code path for it, and
    adding an image variant would mean every caller learning about a case only one
    tool ever produces. */
    Ok(ToolOut::text(format!(
        "SCREENSHOT of {what} \u{2014} {w}x{h} physical pixels.\nCOMPASS_IMAGE:{data_url}"
    )))
}

/// Take the picture. Runs on a blocking thread: capture is a synchronous copy of
/// several megabytes and would otherwise stall every other command.
fn capture(
    window: i64,
    monitor: i64,
    extra: &[String],
) -> Result<(String, String, u32, u32), String> {
    let (image, what) = if window != 0 {
        let title = window_allowed(window, extra)?;
        let wins = xcap::Window::all().map_err(|e| format!("could not list windows: {e}"))?;
        let found = wins
            .into_iter()
            .find(|w| w.id().ok().map(|id| id as i64) == Some(window));
        let Some(w) = found else {
            return Err(
                "that window could not be captured. It may have closed, or be minimised \u{2014} \
                 a minimised window has nothing to photograph."
                    .into(),
            );
        };
        let img = w
            .capture_image()
            .map_err(|e| format!("could not capture that window: {e}"))?;
        (img, format!("the window \u{201C}{title}\u{201D}"))
    } else {
        let mons = xcap::Monitor::all().map_err(|e| format!("could not read the monitors: {e}"))?;
        let idx = if monitor > 0 { monitor as usize } else { 0 };
        let chosen = if monitor > 0 {
            mons.get(idx)
        } else {
            mons.iter()
                .find(|m| m.is_primary().unwrap_or(false))
                .or(mons.first())
        };
        let Some(m) = chosen else {
            return Err(format!("there is no monitor {idx}"));
        };
        let img = m
            .capture_image()
            .map_err(|e| format!("could not capture that monitor: {e}"))?;
        (img, format!("monitor {idx}"))
    };

    let (url, w, h) = encode(image)?;
    Ok((url, what, w, h))
}

/// Downscale and JPEG-encode, reducing quality until it fits.
///
/// A black image is treated as a failure rather than returned. It is what a capture of
/// a protected surface looks like — a DRM-protected video, or a secure desktop — and
/// handing the model a black rectangle invites it to describe what it expects to be
/// there rather than report that it saw nothing.
fn encode(img: xcap::image::RgbaImage) -> Result<(String, u32, u32), String> {
    use image::codecs::jpeg::JpegEncoder;
    use xcap::image::DynamicImage;

    let (w0, h0) = (img.width(), img.height());
    if w0 == 0 || h0 == 0 {
        return Err("the capture came back empty".into());
    }

    if all_black(&img) {
        return Err(
            "the capture came back black, which is what a protected window looks like \u{2014} a \
             security prompt, or protected video. There is nothing to read in it."
                .into(),
        );
    }

    let mut dyn_img = DynamicImage::ImageRgba8(img);
    let longest = w0.max(h0);
    if longest > MAX_SHOT_EDGE {
        let scale = MAX_SHOT_EDGE as f32 / longest as f32;
        let nw = ((w0 as f32) * scale).round().max(1.0) as u32;
        let nh = ((h0 as f32) * scale).round().max(1.0) as u32;
        dyn_img = dyn_img.resize_exact(nw, nh, xcap::image::imageops::FilterType::Triangle);
    }
    let rgb = dyn_img.to_rgb8();
    let (w, h) = (rgb.width(), rgb.height());

    let mut quality = SHOT_QUALITY;
    loop {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut enc = JpegEncoder::new_with_quality(&mut buf, quality);
            enc.encode(rgb.as_raw(), w, h, xcap::image::ExtendedColorType::Rgb8)
                .map_err(|e| format!("could not encode the screenshot: {e}"))?;
        }
        // A data URL is roughly 4/3 the byte length once base64-encoded, so the check
        // is against what will actually be sent rather than what was produced.
        if buf.len() * 4 / 3 <= MAX_SHOT_BYTES || quality <= 35 {
            let b64 = base64_encode(&buf);
            return Ok((format!("data:image/jpeg;base64,{b64}"), w, h));
        }
        quality -= 12;
    }
}

/// Is every pixel black? Sampled rather than exhaustive: a 4K frame is eight million
/// pixels and a protected surface is uniformly black, so a grid of samples answers the
/// question at a fraction of the cost.
fn all_black(img: &xcap::image::RgbaImage) -> bool {
    let (w, h) = (img.width(), img.height());
    if w < 4 || h < 4 {
        return false;
    }
    let step_x = (w / 32).max(1);
    let step_y = (h / 32).max(1);
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            let p = img.get_pixel(x, y);
            if p[0] > 8 || p[1] > 8 || p[2] > 8 {
                return false;
            }
            x += step_x;
        }
        y += step_y;
    }
    true
}

/// Base64, written out rather than taken as a dependency.
///
/// Twenty lines against a crate, for the one place this program needs it. The
/// alternative was another dependency in a binary whose dependency count is a
/// security property.
fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_answers() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_bytes_that_are_not_text() {
        // A JPEG is full of these, so the high bits and the padding both matter.
        assert_eq!(base64_encode(&[0xff, 0xd8, 0xff]), "/9j/");
        assert_eq!(base64_encode(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64_encode(&[0xff]), "/w==");
    }

    #[test]
    fn base64_output_is_always_a_multiple_of_four() {
        for n in 0..40usize {
            let bytes: Vec<u8> = (0..n).map(|i| i as u8).collect();
            let enc = base64_encode(&bytes);
            assert_eq!(enc.len() % 4, 0, "{n} bytes produced {} chars", enc.len());
        }
    }

    #[test]
    fn a_black_frame_is_recognised() {
        let img = xcap::image::RgbaImage::from_pixel(64, 64, xcap::image::Rgba([0, 0, 0, 255]));
        assert!(all_black(&img));
    }

    #[test]
    fn a_frame_with_anything_in_it_is_not_black() {
        let mut img = xcap::image::RgbaImage::from_pixel(64, 64, xcap::image::Rgba([0, 0, 0, 255]));
        // One bright pixel, deliberately not at the origin, so a sampler that only
        // looked at 0,0 would miss it.
        img.put_pixel(32, 32, xcap::image::Rgba([255, 255, 255, 255]));
        assert!(!all_black(&img));
    }

    #[test]
    fn a_nearly_black_frame_is_still_treated_as_content() {
        // A dark theme is not a protected surface. The threshold is deliberately low.
        let img = xcap::image::RgbaImage::from_pixel(64, 64, xcap::image::Rgba([12, 12, 14, 255]));
        assert!(!all_black(&img));
    }

    #[test]
    fn a_tiny_frame_is_never_judged_black() {
        // Too few pixels to sample meaningfully; refusing it would be a false alarm.
        let img = xcap::image::RgbaImage::from_pixel(2, 2, xcap::image::Rgba([0, 0, 0, 255]));
        assert!(!all_black(&img));
    }

    #[test]
    fn the_wait_bound_is_enforced_in_both_directions() {
        assert_eq!(0u64.clamp(50, MAX_WAIT_MS), 50);
        assert_eq!(999_999u64.clamp(50, MAX_WAIT_MS), MAX_WAIT_MS);
        assert_eq!(2_000u64.clamp(50, MAX_WAIT_MS), 2_000);
    }

    #[test]
    fn an_excluded_window_is_omitted_from_a_listing_rather_than_marked() {
        // Constructed rather than enumerated, since a test cannot rely on what is open.
        // What is asserted is the filter, which is the part with a security consequence:
        // a listing saying "1Password (hidden)" has already leaked that it is open.
        let raw = vec![
            (
                WindowInfo {
                    id: 1,
                    title: "Chemistry.docx - Word".into(),
                    process: "WINWORD.EXE".into(),
                    x: 0,
                    y: 0,
                    width: 800,
                    height: 600,
                    focused: true,
                    minimised: false,
                },
                "OpusApp".to_string(),
            ),
            (
                WindowInfo {
                    id: 2,
                    title: "1Password".into(),
                    process: "1Password.exe".into(),
                    x: 0,
                    y: 0,
                    width: 400,
                    height: 700,
                    focused: false,
                    minimised: false,
                },
                "Chrome_WidgetWin_1".to_string(),
            ),
        ];
        let kept: Vec<_> = raw
            .into_iter()
            .filter(|(i, c)| !grant::is_blocked(&i.title, c, &[]))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].id, 1);
    }
}
