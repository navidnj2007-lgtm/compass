//! Moving the mouse and pressing keys. The most dangerous file in this program.
//!
//! Everything else here is bounded by something structural. A file tool can only reach
//! the folders in the policy; the browser tools can only reach a Chrome profile Compass
//! owns. Synthetic input has no equivalent: `SendInput` is indistinguishable from a
//! person at the keyboard, to every application on the machine, including ones Compass
//! has no other route to. There is no sandbox to put it in.
//!
//! So the controls are all about *when* and *what*, and there are six of them. Every
//! one has to pass:
//!
//!   1. A live session grant, which only a person can start and which expires. See
//!      `grant.rs`.
//!   2. A per-event claim against the grant's step budget and its per-minute rate
//!      limit, so a runaway loop stops itself.
//!   3. Coordinates inside a real monitor. A click at 40,000 pixels is a bug or an
//!      attempt to reach something that is not on screen.
//!   4. A target window that is not on the exclusion list — checked at the coordinate,
//!      so it is the window that will actually receive the click rather than whichever
//!      one was named.
//!   5. For typing, the secret filter below.
//!   6. `consent::require_always` — a Windows dialog naming the window and, for
//!      typing, the literal text. No policy value can suppress it, because it takes no
//!      policy.
//!
//! WHAT IS NOT ATTEMPTED, AND WHY
//!
//! There is no way to send arbitrary scancodes or virtual-key codes. `pc.hotkey` takes
//! names from a fixed list. That rules out a great deal of legitimate use — no F13, no
//! media keys, no numpad — and it also rules out the thing that matters: an agent that
//! can send any key can send Ctrl+Shift+Esc, or Win+R followed by a command, and at
//! that point every other control in this program is decoration.
//!
//! There is no `pc.paste`. Putting text on the clipboard and pressing Ctrl+V would be a
//! way to type something the consent dialog never showed, because the dialog would have
//! named the keystroke and not the payload.

use crate::agent::{Agent, ToolOut};
use crate::consent;
use crate::tools::screen;
use serde::Deserialize;
use tauri::{AppHandle, State};

/// Keys `pc.hotkey` may press, and nothing else.
///
/// Deliberately short. Everything here is something a person would recognise from a
/// menu — save, copy, undo, switch window — and the list grows only when a real task
/// needs it. Note the absences: no Win+R, no Ctrl+Alt+Del, no F-keys beyond the ones
/// applications actually bind, and no way to express a raw code.
pub const ALLOWED_KEYS: &[&str] = &[
    // Modifiers, only ever as part of a combination.
    "ctrl",
    "alt",
    "shift",
    "win",
    // Editing and navigation.
    "enter",
    "tab",
    "escape",
    "space",
    "backspace",
    "delete",
    "insert",
    "home",
    "end",
    "pageup",
    "pagedown",
    "up",
    "down",
    "left",
    "right",
    // Letters and digits, for combinations like ctrl+s.
    "a",
    "b",
    "c",
    "d",
    "e",
    "f",
    "g",
    "h",
    "i",
    "j",
    "k",
    "l",
    "m",
    "n",
    "o",
    "p",
    "q",
    "r",
    "s",
    "t",
    "u",
    "v",
    "w",
    "x",
    "y",
    "z",
    "0",
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    // The function keys applications really bind.
    "f1",
    "f2",
    "f3",
    "f4",
    "f5",
    "f6",
    "f11",
    "f12",
];

/// Combinations refused however they are spelled, because their effect is not "a
/// keystroke in the focused application" but "something about the machine".
///
/// Checked on the normalised, sorted combination, so `alt+ctrl+delete` and
/// `ctrl+alt+delete` are the same thing to it.
const REFUSED_COMBOS: &[&[&str]] = &[
    &["ctrl", "alt", "delete"],   // the secure attention sequence
    &["ctrl", "shift", "escape"], // task manager
    &["win", "r"],                // run dialog
    &["win", "e"],                // explorer, harmless but a stepping stone
    &["win", "x"],                // the admin menu
    &["win", "l"],                // lock, which would end the session mid-task
    &["alt", "f4"],               // close, too easy to aim at the wrong window
];

#[derive(Debug, Deserialize)]
pub struct MoveReq {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Deserialize)]
pub struct ClickReq {
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub button: String,
    #[serde(default)]
    pub double: bool,
}

#[derive(Debug, Deserialize)]
pub struct DragReq {
    pub from_x: i32,
    pub from_y: i32,
    pub to_x: i32,
    pub to_y: i32,
    #[serde(default)]
    pub button: String,
}

#[derive(Debug, Deserialize)]
pub struct ScrollReq {
    pub x: i32,
    pub y: i32,
    /// Positive scrolls up, negative down. Clamped.
    pub amount: i32,
}

#[derive(Debug, Deserialize)]
pub struct TypeReq {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct HotkeyReq {
    /// A combination like "ctrl+s". Names only, from ALLOWED_KEYS.
    pub keys: String,
}

#[derive(Debug, Deserialize)]
pub struct FocusReq {
    pub window: i64,
}

/// Characters typed in one call. A paragraph is fine; a file is not, and a very long
/// string is how someone would try to make the consent dialog unreadable.
pub const MAX_TYPE_CHARS: usize = 2_000;

/// Scroll notches in one call.
const MAX_SCROLL: i32 = 20;

/* ── the secret filter ───────────────────────────────────────────────
The rule is that Compass never types a password, a card number, a one-time code or a
recovery code, and never accepts one from the model. This is where that is enforced.

It is a pattern matcher, which means it is imprecise, and the imprecision is aimed
deliberately. A false positive costs a refused sentence and an explanation. A false
negative costs a card number typed into a web form by an agent following instructions
from a page it read. The thresholds are therefore set to catch too much.

It is not, and cannot be, complete. Someone determined to have the agent type a secret
can spell it out in words. What it stops is the realistic case: a model that has read a
credential out of a file or a page and is helpfully filling in a form with it. */

/// Would typing this be typing a secret? Returns why, so the refusal can explain.
pub fn looks_secret(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();

    // A card number: 13–19 digits, allowing the spaces and dashes people write them
    // with, and confirmed by the Luhn check so an order number or a phone number is
    // not caught.
    if let Some(found) = find_card_number(text) {
        return Some(format!(
            "that contains what looks like a payment card number ({}\u{2026}), and Compass never \
             types card details",
            &found[..found.len().min(4)]
        ));
    }

    // A one-time code: a short run of digits near a word that means "code". The
    // proximity test is what stops every number in every sentence tripping it.
    if let Some(word) = code_word_near_digits(&lower) {
        return Some(format!(
            "that looks like a one-time or verification code (it mentions \u{201C}{word}\u{201D} \
             next to a short number), and Compass never types those \u{2014} they are the one thing \
             an attacker most wants typed somewhere"
        ));
    }

    // Anything self-describing as a credential.
    for word in [
        "password",
        "passphrase",
        "recovery code",
        "recovery key",
        "backup code",
        "seed phrase",
        "private key",
        "api key",
        "secret key",
        "cvv",
        "cvc",
        "pin code",
        "security code",
    ] {
        if lower.contains(word) {
            return Some(format!(
                "that mentions \u{201C}{word}\u{201D}. Compass will not type credentials \u{2014} if \
                 something needs one, he types it himself"
            ));
        }
    }

    // A private key block, which is unmistakable and worth naming separately.
    if lower.contains("-----begin") && lower.contains("private key") {
        return Some("that is a private key".into());
    }

    None
}

/// The first Luhn-valid 13-to-19 digit run in the text, digits only.
fn find_card_number(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // Collect a run, allowing single separators inside it.
        let mut digits = String::new();
        let mut j = i;
        let mut last_was_sep = false;
        while j < chars.len() && digits.len() < 19 {
            let c = chars[j];
            if c.is_ascii_digit() {
                digits.push(c);
                last_was_sep = false;
            } else if (c == ' ' || c == '-') && !last_was_sep && !digits.is_empty() {
                last_was_sep = true;
            } else {
                break;
            }
            j += 1;
        }
        if digits.len() >= 13 && luhn(&digits) {
            return Some(digits);
        }
        // Advance past this run rather than one character, or a 19-digit number costs
        // nineteen overlapping checks.
        i = j.max(i + 1);
    }
    None
}

fn luhn(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for c in digits.chars().rev() {
        let Some(d) = c.to_digit(10) else {
            return false;
        };
        let v = if double {
            let x = d * 2;
            if x > 9 {
                x - 9
            } else {
                x
            }
        } else {
            d
        };
        sum += v;
        double = !double;
    }
    sum % 10 == 0
}

/// A word meaning "code" within 40 characters of a 4-to-8 digit run.
///
/// Proximity rather than mere presence, because "I got 6 marks on question 4" mentions
/// numbers and "the postcode is 2100" mentions a code, and neither is a secret. Forty
/// characters is about the distance across a phrase like "your verification code is
/// 402913".
fn code_word_near_digits(lower: &str) -> Option<String> {
    const WORDS: &[&str] = &[
        "code",
        "otp",
        "2fa",
        "two-factor",
        "verification",
        "authenticator",
        "token",
    ];
    let bytes = lower.as_bytes();

    for w in WORDS {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(w) {
            let at = from + rel;
            let lo = at.saturating_sub(40);
            let hi = (at + w.len() + 40).min(bytes.len());
            if has_short_digit_run(&lower[lo..hi]) {
                return Some((*w).to_string());
            }
            from = at + w.len();
        }
    }
    None
}

fn has_short_digit_run(s: &str) -> bool {
    let mut run = 0usize;
    for c in s.chars() {
        if c.is_ascii_digit() {
            run += 1;
            if (4..=8).contains(&run) {
                // Keep scanning: a longer run is a different thing (an account number,
                // a card, a year range) and is handled elsewhere.
                continue;
            }
        } else {
            if (4..=8).contains(&run) {
                return true;
            }
            run = 0;
        }
    }
    (4..=8).contains(&run)
}

/* ── the hotkey allow-list ───────────────────────────────────────── */

/// Parse and validate a combination. Returns the normalised key names.
pub fn parse_keys(spec: &str) -> Result<Vec<String>, String> {
    let raw = spec.trim().to_ascii_lowercase();
    if raw.is_empty() {
        return Err("no keys were named".into());
    }
    if raw.len() > 40 {
        return Err("that is not a key combination".into());
    }

    let mut keys: Vec<String> = Vec::new();
    for part in raw.split('+') {
        let k = part.trim();
        if k.is_empty() {
            return Err(format!("\u{201C}{spec}\u{201D} has an empty part in it"));
        }
        let k = match k {
            // The spellings people actually write.
            "control" => "ctrl",
            "esc" => "escape",
            "return" => "enter",
            "del" => "delete",
            "ins" => "insert",
            "pgup" => "pageup",
            "pgdn" | "pgdown" => "pagedown",
            "super" | "meta" | "cmd" | "windows" => "win",
            other => other,
        };
        if !ALLOWED_KEYS.contains(&k) {
            return Err(format!(
                "\u{201C}{k}\u{201D} is not a key Compass will press. It only presses named keys \
                 from a fixed list \u{2014} letters, digits, the arrows, and the usual modifiers."
            ));
        }
        if !keys.iter().any(|x| x == k) {
            keys.push(k.to_string());
        }
    }
    if keys.len() > 4 {
        return Err("that is more keys at once than any real shortcut uses".into());
    }

    let mut sorted = keys.clone();
    sorted.sort();
    for refused in REFUSED_COMBOS {
        let mut want: Vec<String> = refused.iter().map(|s| s.to_string()).collect();
        want.sort();
        if sorted == want {
            return Err(format!(
                "Compass will not press {} \u{2014} that is a Windows command rather than a \
                 keystroke in the program he is working in.",
                refused.join("+")
            ));
        }
    }
    Ok(keys)
}

/* ── coordinates ─────────────────────────────────────────────────── */

/// Is this point on a real monitor? Returns the monitor index, or says why not.
///
/// The virtual desktop is not a rectangle when monitors are of different sizes or are
/// offset, so this checks each monitor rather than a bounding box: a point in the
/// notional gap beside a smaller second screen is not on any screen, and clicking there
/// does nothing at best.
pub fn point_on_screen(x: i32, y: i32) -> Result<usize, String> {
    let mons = screen::monitors()?;
    for m in &mons {
        if x >= m.x && y >= m.y && x < m.x + m.width as i32 && y < m.y + m.height as i32 {
            return Ok(m.index);
        }
    }
    let described: Vec<String> = mons
        .iter()
        .map(|m| format!("{}x{} at {},{}", m.width, m.height, m.x, m.y))
        .collect();
    Err(format!(
        "{x},{y} is not on any screen. The screens are: {}. Use pc.list_monitors and \
         pc.screenshot to work out where things actually are.",
        described.join("; ")
    ))
}

/* ── sending the input ───────────────────────────────────────────────
Raw `SendInput` through the `windows` crate rather than a wrapper.

`enigo` was the alternative and was rejected for a specific reason rather than taste:
it exposes a `Key::Raw(u16)` and a `Key::Other`, so an agent that could reach it could
send any virtual-key code, and the allow-list above would be a suggestion. What is
needed here is a deliberately incomplete keyboard, and the way to have one is to write
only the parts that are wanted.

Mouse coordinates go through `SendInput` in absolute mode, which uses a 0–65535 range
over the *primary* monitor unless VIRTUALDESK is set — a detail that produces clicks on
the wrong screen in a multi-monitor setup and is the reason the flag is set explicitly
below. */

#[cfg(windows)]
mod send {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
        MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
        MOUSEINPUT, VIRTUAL_KEY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// One wheel notch, as Windows defines it. Written out rather than imported: it is a
    /// documented constant that has moved between modules of the `windows` crate across
    /// versions, and a literal that cannot move is better than an import that can.
    const WHEEL_DELTA: i32 = 120;

    /// Physical pixels to the 0–65535 space `SendInput` wants, over the whole virtual
    /// desktop rather than the primary monitor — which is what `MOUSEEVENTF_VIRTUALDESK`
    /// selects, and without it every click on a second monitor lands on the first.
    fn to_absolute(x: i32, y: i32) -> (i32, i32) {
        unsafe {
            let ox = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let oy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
            let nx = ((x - ox) as i64 * 65535 / w as i64) as i32;
            let ny = ((y - oy) as i64 * 65535 / h as i64) as i32;
            (nx.clamp(0, 65535), ny.clamp(0, 65535))
        }
    }

    fn mouse(dx: i32, dy: i32, flags: u32, data: i32) -> Result<(), String> {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(
                "Windows refused the input. This happens when a window running as \
                        administrator has focus: Compass is not elevated, so it cannot send \
                        keystrokes to one that is."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn move_to(x: i32, y: i32) -> Result<(), String> {
        let (ax, ay) = to_absolute(x, y);
        mouse(
            ax,
            ay,
            MOUSEEVENTF_MOVE.0 | MOUSEEVENTF_ABSOLUTE.0 | MOUSEEVENTF_VIRTUALDESK.0,
            0,
        )
    }

    pub fn button(which: &str, down: bool) -> Result<(), String> {
        let flag = match (which, down) {
            ("right", true) => MOUSEEVENTF_RIGHTDOWN,
            ("right", false) => MOUSEEVENTF_RIGHTUP,
            ("middle", true) => MOUSEEVENTF_MIDDLEDOWN,
            ("middle", false) => MOUSEEVENTF_MIDDLEUP,
            (_, true) => MOUSEEVENTF_LEFTDOWN,
            (_, false) => MOUSEEVENTF_LEFTUP,
        };
        mouse(0, 0, flag.0, 0)
    }

    pub fn wheel(notches: i32) -> Result<(), String> {
        mouse(0, 0, MOUSEEVENTF_WHEEL.0, notches * WHEEL_DELTA)
    }

    /// Type one character as a Unicode event.
    ///
    /// `KEYEVENTF_UNICODE` rather than a virtual-key lookup, deliberately: it means the
    /// text arrives as written whatever keyboard layout is active. A Danish layout would
    /// otherwise turn a typed `/` into something else, and an agent that types different
    /// characters than the ones the consent dialog showed is worse than one that cannot
    /// type at all.
    pub fn unicode(ch: u16, down: bool) -> Result<(), String> {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: ch,
                    dwFlags: if down {
                        KEYEVENTF_UNICODE
                    } else {
                        KEYBD_EVENT_FLAGS(KEYEVENTF_UNICODE.0 | KEYEVENTF_KEYUP.0)
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(
                "Windows refused the keystroke, which usually means an elevated window \
                        has focus."
                    .into(),
            );
        }
        Ok(())
    }

    /// The virtual-key code for one allow-listed name. Exhaustive over ALLOWED_KEYS by
    /// construction: a name with no code here cannot be pressed, and the test above
    /// asserts every allow-listed name parses, while this returning None makes an
    /// unmapped one a refusal rather than a wrong key.
    pub fn vk_of(name: &str) -> Option<u16> {
        use windows::Win32::UI::Input::KeyboardAndMouse as k;
        let vk = match name {
            "ctrl" => k::VK_CONTROL,
            "alt" => k::VK_MENU,
            "shift" => k::VK_SHIFT,
            "win" => k::VK_LWIN,
            "enter" => k::VK_RETURN,
            "tab" => k::VK_TAB,
            "escape" => k::VK_ESCAPE,
            "space" => k::VK_SPACE,
            "backspace" => k::VK_BACK,
            "delete" => k::VK_DELETE,
            "insert" => k::VK_INSERT,
            "home" => k::VK_HOME,
            "end" => k::VK_END,
            "pageup" => k::VK_PRIOR,
            "pagedown" => k::VK_NEXT,
            "up" => k::VK_UP,
            "down" => k::VK_DOWN,
            "left" => k::VK_LEFT,
            "right" => k::VK_RIGHT,
            "f1" => k::VK_F1,
            "f2" => k::VK_F2,
            "f3" => k::VK_F3,
            "f4" => k::VK_F4,
            "f5" => k::VK_F5,
            "f6" => k::VK_F6,
            "f11" => k::VK_F11,
            "f12" => k::VK_F12,
            other => {
                let b = other.as_bytes();
                if b.len() == 1 && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit()) {
                    // 'a'..'z' and '0'..'9' map to their uppercase ASCII value.
                    return Some(b[0].to_ascii_uppercase() as u16);
                }
                return None;
            }
        };
        Some(vk.0)
    }

    pub fn key(vk: u16, down: bool) -> Result<(), String> {
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: if down {
                        KEYBD_EVENT_FLAGS(0)
                    } else {
                        KEYEVENTF_KEYUP
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(
                "Windows refused the keystroke, which usually means an elevated window \
                        has focus."
                    .into(),
            );
        }
        Ok(())
    }

    pub fn focus(id: i64) -> Result<(), String> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            SetForegroundWindow, ShowWindow, SW_RESTORE,
        };
        let hwnd = HWND(id as *mut core::ffi::c_void);
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            if !SetForegroundWindow(hwnd).as_bool() {
                return Err(
                    "Windows would not bring that window to the front. It does that when \
                            the window belongs to an elevated program, or when something else \
                            is holding focus."
                        .into(),
                );
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod send {
    pub fn move_to(_x: i32, _y: i32) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
    pub fn button(_w: &str, _d: bool) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
    pub fn wheel(_n: i32) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
    pub fn unicode(_c: u16, _d: bool) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
    pub fn vk_of(_n: &str) -> Option<u16> {
        None
    }
    pub fn key(_v: u16, _d: bool) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
    pub fn focus(_id: i64) -> Result<(), String> {
        Err("computer control is Windows-only".into())
    }
}

/* ── the gate every acting command passes ────────────────────────── */

/// What is at this point, and may it be touched?
///
/// Checked at the coordinate rather than against a named window, because the click will
/// land on whatever is under the pointer. A model that names a permitted window and
/// clicks at coordinates over a password manager on top of it must be refused, and only
/// the coordinate knows that.
fn target_at(x: i32, y: i32, extra: &[String]) -> Result<String, String> {
    point_on_screen(x, y)?;
    for w in screen::visible_windows(extra) {
        if w.minimised {
            continue;
        }
        if x >= w.x && y >= w.y && x < w.x + w.width && y < w.y + w.height {
            return Ok(w.title);
        }
    }
    /* Nothing recognised: the desktop, or a window with no title. Permitted — refusing
    would make the feature unusable on a normal desktop — but named honestly so the
    consent dialog does not claim to know what it is about to click. */
    Ok("(the desktop, or an untitled window)".into())
}

/// Every acting command starts here: a live grant, a claim against its budgets, and a
/// point on a real screen belonging to a window that is not excluded.
async fn gate(state: &Agent, x: i32, y: i32) -> Result<String, String> {
    state.grants.claim()?;
    let pol = state.policy();
    target_at(x, y, &pol.blocked_windows)
}

/* ── the commands ────────────────────────────────────────────────── */

/// Bring a window to the front. Medium: it changes what has focus, which is noticeable
/// and reversible, and it is the necessary first step of almost every real task.
#[tauri::command]
pub async fn pc_focus_window(
    app: AppHandle,
    state: State<'_, Agent>,
    req: FocusReq,
) -> Result<ToolOut, String> {
    let policy = state.policy();
    let out = async {
        state.grants.claim()?;
        let title = screen::window_allowed(req.window, &policy.blocked_windows)?;
        consent::require(
            &app,
            &policy,
            crate::policy::Risk::Medium,
            "Switch window",
            &format!("The assistant wants to bring this window to the front:\n\n{title}"),
        )
        .await?;
        send::focus(req.window)?;
        Ok(ToolOut::done_with(format!(
            "Brought \u{201C}{title}\u{201D} to the front."
        )))
    }
    .await;
    state.audit.record(
        "pc.focus_window",
        out.is_ok(),
        format!("Focus window {}", req.window),
        out.as_ref().err().cloned(),
        false,
    );
    out
}

/// Move the pointer. The only acting command with no dialog: moving the pointer changes
/// nothing, and a prompt before every move would make a move-then-click sequence need
/// two confirmations for one action.
#[tauri::command]
pub async fn pc_mouse_move(state: State<'_, Agent>, req: MoveReq) -> Result<ToolOut, String> {
    let out = async {
        let what = gate(&state, req.x, req.y).await?;
        send::move_to(req.x, req.y)?;
        Ok(ToolOut::done_with(format!(
            "Pointer moved to {},{} \u{2014} over {what}.",
            req.x, req.y
        )))
    }
    .await;
    state.audit.record(
        "pc.mouse_move",
        out.is_ok(),
        format!("Pointer to {},{}", req.x, req.y),
        out.as_ref().err().cloned(),
        false,
    );
    out
}

#[tauri::command]
pub async fn pc_scroll(state: State<'_, Agent>, req: ScrollReq) -> Result<ToolOut, String> {
    let out = async {
        let what = gate(&state, req.x, req.y).await?;
        let n = req.amount.clamp(-MAX_SCROLL, MAX_SCROLL);
        if n == 0 {
            return Err("a scroll of nothing does nothing".to_string());
        }
        send::move_to(req.x, req.y)?;
        send::wheel(n)?;
        Ok(ToolOut::done_with(format!(
            "Scrolled {} over {what}.",
            if n > 0 {
                format!("up {n}")
            } else {
                format!("down {}", -n)
            }
        )))
    }
    .await;
    state.audit.record(
        "pc.scroll",
        out.is_ok(),
        format!("Scroll {} at {},{}", req.amount, req.x, req.y),
        out.as_ref().err().cloned(),
        false,
    );
    out
}

#[tauri::command]
pub async fn pc_click(
    app: AppHandle,
    state: State<'_, Agent>,
    req: ClickReq,
) -> Result<ToolOut, String> {
    let out = async {
        let what = gate(&state, req.x, req.y).await?;
        let button = match req.button.as_str() {
            "right" => "right",
            "middle" => "middle",
            "" | "left" => "left",
            other => return Err(format!("\u{201C}{other}\u{201D} is not a mouse button")),
        };

        /* require_always, not require. It takes no policy, so there is no field anyone
        could set and no argument anyone could pass that removes this dialog. The
        window it names is the one under the pointer, not the one that was asked
        for. */
        consent::require_always(
            &app,
            "Click",
            &format!(
                "The assistant wants to {}click here:\n\n{}\n\nat {},{} on your screen.",
                if req.double { "double-" } else { "" },
                what,
                req.x,
                req.y
            ),
        )
        .await?;

        send::move_to(req.x, req.y)?;
        send::button(button, true)?;
        send::button(button, false)?;
        if req.double {
            send::button(button, true)?;
            send::button(button, false)?;
        }
        Ok(ToolOut::done_with(format!(
            "{}Clicked {button} at {},{} on {what}. Take a screenshot to see what changed.",
            if req.double { "Double-" } else { "" },
            req.x,
            req.y
        )))
    }
    .await;
    state.audit.record(
        "pc.click",
        out.is_ok(),
        format!("Click at {},{}", req.x, req.y),
        out.as_ref().err().cloned(),
        true,
    );
    out
}

#[tauri::command]
pub async fn pc_drag(
    app: AppHandle,
    state: State<'_, Agent>,
    req: DragReq,
) -> Result<ToolOut, String> {
    let out = async {
        // Both ends are gated: a drag that starts somewhere permitted and ends over a
        // password manager is still a drop onto a password manager.
        let from = gate(&state, req.from_x, req.from_y).await?;
        let to = target_at(req.to_x, req.to_y, &state.policy().blocked_windows)?;
        let button = if req.button == "right" {
            "right"
        } else {
            "left"
        };

        consent::require_always(
            &app,
            "Drag",
            &format!(
                "The assistant wants to drag from {},{} to {},{}.\n\nFrom: {from}\nTo: {to}",
                req.from_x, req.from_y, req.to_x, req.to_y
            ),
        )
        .await?;

        send::move_to(req.from_x, req.from_y)?;
        send::button(button, true)?;
        // A few intermediate positions: applications that track movement rather than
        // just the endpoints ignore a drag that teleports.
        for i in 1..=8 {
            let x = req.from_x + (req.to_x - req.from_x) * i / 8;
            let y = req.from_y + (req.to_y - req.from_y) * i / 8;
            send::move_to(x, y)?;
        }
        send::button(button, false)?;
        Ok(ToolOut::done_with(format!(
            "Dragged from {},{} to {},{}. Take a screenshot to see what changed.",
            req.from_x, req.from_y, req.to_x, req.to_y
        )))
    }
    .await;
    state.audit.record(
        "pc.drag",
        out.is_ok(),
        format!(
            "Drag {},{} to {},{}",
            req.from_x, req.from_y, req.to_x, req.to_y
        ),
        out.as_ref().err().cloned(),
        true,
    );
    out
}

#[tauri::command]
pub async fn pc_type(
    app: AppHandle,
    state: State<'_, Agent>,
    req: TypeReq,
) -> Result<ToolOut, String> {
    let out = async {
        state.grants.claim()?;

        if req.text.is_empty() {
            return Err("there was nothing to type".to_string());
        }
        if req.text.chars().count() > MAX_TYPE_CHARS {
            return Err(format!(
                "that is {} characters and Compass types at most {MAX_TYPE_CHARS} at once. A very \
                 long string also makes the confirmation unreadable, which defeats its purpose.",
                req.text.chars().count()
            ));
        }

        /* The secret filter, before consent. Refusing here rather than showing a dialog
        means the text never appears in a prompt he might approve out of habit, and it
        is never echoed anywhere. */
        if let Some(why) = looks_secret(&req.text) {
            return Err(format!("Compass will not type that: {why}."));
        }

        let pol = state.policy();
        let focused = screen::visible_windows(&pol.blocked_windows)
            .into_iter()
            .find(|w| w.focused)
            .map(|w| w.title)
            .unwrap_or_else(|| "(whatever has focus)".into());

        /* The literal text, in the dialog. Not a character count and not a summary: the
        whole reason this prompt exists is so he sees what is about to be typed. */
        consent::require_always(
            &app,
            "Type",
            &format!(
                "The assistant wants to type this into \u{201C}{focused}\u{201D}:\n\n{}\n\n\
                 It will go wherever the cursor is.",
                req.text
            ),
        )
        .await?;

        for ch in req.text.encode_utf16() {
            send::unicode(ch, true)?;
            send::unicode(ch, false)?;
        }
        Ok(ToolOut::done_with(format!(
            "Typed {} character(s) into \u{201C}{focused}\u{201D}. Take a screenshot to check it \
             went where you expected.",
            req.text.chars().count()
        )))
    }
    .await;

    /* The audit records the length, never the text. A log that quoted everything typed
    would be a transcript of everything he had the agent write, sitting in a file for
    ever. */
    state.audit.record(
        "pc.type",
        out.is_ok(),
        format!("Typed {} character(s)", req.text.chars().count()),
        out.as_ref().err().cloned(),
        true,
    );
    out
}

#[tauri::command]
pub async fn pc_hotkey(
    app: AppHandle,
    state: State<'_, Agent>,
    req: HotkeyReq,
) -> Result<ToolOut, String> {
    let out = async {
        state.grants.claim()?;
        let keys = parse_keys(&req.keys)?;

        let pol = state.policy();
        let focused = screen::visible_windows(&pol.blocked_windows)
            .into_iter()
            .find(|w| w.focused)
            .map(|w| w.title)
            .unwrap_or_else(|| "(whatever has focus)".into());

        consent::require_always(
            &app,
            "Press keys",
            &format!(
                "The assistant wants to press {} in \u{201C}{focused}\u{201D}.",
                keys.join("+")
            ),
        )
        .await?;

        let mut codes = Vec::new();
        for k in &keys {
            let Some(vk) = send::vk_of(k) else {
                return Err(format!(
                    "Compass has no way to press \u{201C}{k}\u{201D}, so nothing was pressed."
                ));
            };
            codes.push(vk);
        }
        // Down in order, up in reverse, or a modifier is released before the key it
        // modifies and the shortcut becomes two separate keystrokes.
        for vk in &codes {
            send::key(*vk, true)?;
        }
        for vk in codes.iter().rev() {
            send::key(*vk, false)?;
        }
        Ok(ToolOut::done_with(format!(
            "Pressed {} in \u{201C}{focused}\u{201D}. Take a screenshot to see what it did.",
            keys.join("+")
        )))
    }
    .await;
    state.audit.record(
        "pc.hotkey",
        out.is_ok(),
        format!("Pressed {}", req.keys),
        out.as_ref().err().cloned(),
        true,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ── the secret filter ───────────────────────────────────────── */

    #[test]
    fn a_card_number_is_refused_however_it_is_spaced() {
        // Luhn-valid test numbers.
        for s in [
            "4242424242424242",
            "4242 4242 4242 4242",
            "4242-4242-4242-4242",
            "my card is 4242 4242 4242 4242 thanks",
            "5555555555554444",
        ] {
            assert!(looks_secret(s).is_some(), "{s} should be refused");
        }
    }

    #[test]
    fn an_ordinary_long_number_is_not_a_card() {
        // Not Luhn-valid, so an order number or an ID does not trip it.
        for s in [
            "order 1234567890123",
            "1111111111111",
            "the reference is 9876543210988",
        ] {
            let got = looks_secret(s);
            assert!(
                got.as_deref().map(|m| !m.contains("card")).unwrap_or(true),
                "{s} was wrongly called a card: {got:?}"
            );
        }
    }

    #[test]
    fn a_luhn_valid_reference_number_is_refused_and_that_is_the_intended_trade() {
        // Found by this test failing on a fixture I had assumed was innocent:
        // 9876543210987 is 13 digits and happens to satisfy Luhn, so it is
        // indistinguishable from a card number by any test that does not know what it
        // refers to. It is refused.
        //
        // That is the accepted trade, stated here rather than discovered later: a false
        // refusal costs one sentence of explanation and he can type the number himself,
        // whereas a false allow costs a card number typed into a form by an agent acting
        // on something it read in a file. The filter is aimed to catch too much.
        let why = looks_secret("the reference is 9876543210987").unwrap();
        assert!(why.contains("card"), "{why}");
    }

    #[test]
    fn a_one_time_code_is_refused() {
        for s in [
            "your verification code is 402913",
            "the code is 8213",
            "OTP: 553219",
            "enter 402913 as your 2FA code",
            "authenticator shows 118842",
        ] {
            assert!(looks_secret(s).is_some(), "{s} should be refused");
        }
    }

    #[test]
    fn numbers_in_ordinary_sentences_are_not_codes() {
        // These are the false positives that would make the feature useless, so they
        // are pinned as firmly as the refusals.
        for s in [
            "I got 6 marks on question 4",
            "the meeting is at 14:30 in room 212",
            "chapter 12, pages 340 to 356",
            "add 45 minutes to the task",
            "Le Chatelier's principle",
            "the answer is 0.0821",
        ] {
            assert!(
                looks_secret(s).is_none(),
                "{s} was wrongly refused: {:?}",
                looks_secret(s)
            );
        }
    }

    #[test]
    fn a_code_word_far_from_a_number_is_not_a_code() {
        // "code" and a number in the same paragraph but not the same phrase.
        let s = "Write the code for the experiment. \
                 Then, much later in this rather long sentence, mention 4023 as a page reference.";
        assert!(looks_secret(s).is_none(), "{:?}", looks_secret(s));
    }

    #[test]
    fn anything_self_describing_as_a_credential_is_refused() {
        for s in [
            "my password is hunter2",
            "PASSWORD",
            "here is the recovery code",
            "seed phrase: abandon abandon",
            "cvv 123",
            "the api key is sk-abc",
        ] {
            assert!(looks_secret(s).is_some(), "{s} should be refused");
        }
    }

    #[test]
    fn a_private_key_block_is_refused() {
        assert!(looks_secret("-----BEGIN RSA PRIVATE KEY-----").is_some());
    }

    #[test]
    fn the_refusal_explains_itself_and_does_not_echo_the_secret() {
        let why = looks_secret("4242 4242 4242 4242").unwrap();
        assert!(why.contains("card"), "{why}");
        // At most the first four digits, so the message is useful without repeating
        // the number into a log or a chat transcript.
        assert!(!why.contains("4242424242424242"), "{why}");
        assert!(!why.contains("4242 4242 4242 4242"), "{why}");
    }

    #[test]
    fn ordinary_typing_passes() {
        for s in [
            "Le Chatelier's principle shifts the equilibrium.",
            "Dear Ms Hansen,\n\nThank you for the extension.",
            "## Electrochemistry\n\n- oxidation is loss",
            "",
        ] {
            assert!(looks_secret(s).is_none(), "{s:?} was wrongly refused");
        }
    }

    /* ── the hotkey allow-list ───────────────────────────────────── */

    #[test]
    fn ordinary_shortcuts_are_allowed() {
        for s in [
            "ctrl+s",
            "ctrl+c",
            "ctrl+v",
            "alt+tab",
            "ctrl+shift+n",
            "enter",
            "escape",
            "f5",
        ] {
            assert!(
                parse_keys(s).is_ok(),
                "{s} should be allowed: {:?}",
                parse_keys(s)
            );
        }
    }

    #[test]
    fn common_spellings_are_understood() {
        assert_eq!(parse_keys("Control+S").unwrap(), vec!["ctrl", "s"]);
        assert_eq!(parse_keys("CTRL+Return").unwrap(), vec!["ctrl", "enter"]);
        assert_eq!(parse_keys("cmd+a").unwrap(), vec!["win", "a"]);
        assert_eq!(parse_keys("esc").unwrap(), vec!["escape"]);
    }

    #[test]
    fn a_raw_code_cannot_be_expressed() {
        // The whole point of an allow-list: there is no syntax for "key 0x5B".
        for s in [
            "0x5b",
            "vk91",
            "scancode:91",
            "\u{e0}",
            "f13",
            "printscreen",
            "numpad0",
        ] {
            assert!(parse_keys(s).is_err(), "{s} should be refused");
        }
    }

    #[test]
    fn the_machine_commands_are_refused_in_any_order() {
        for s in [
            "ctrl+alt+delete",
            "alt+ctrl+delete",
            "delete+ctrl+alt",
            "win+r",
            "r+win",
            "ctrl+shift+escape",
            "win+l",
            "alt+f4",
        ] {
            assert!(
                parse_keys(s).is_err(),
                "{s} should be refused: {:?}",
                parse_keys(s)
            );
        }
    }

    #[test]
    fn a_refused_combination_says_what_it_refused() {
        let why = parse_keys("win+r").unwrap_err();
        assert!(why.contains("win+r"), "{why}");
        assert!(why.contains("Windows command"), "{why}");
    }

    #[test]
    fn malformed_combinations_are_refused_legibly() {
        for s in [
            "",
            "   ",
            "ctrl+",
            "+s",
            "ctrl++s",
            "ctrl+alt+shift+win+a+b",
        ] {
            assert!(parse_keys(s).is_err(), "{s:?} should be refused");
        }
    }

    #[test]
    fn a_duplicate_modifier_is_collapsed_rather_than_refused() {
        assert_eq!(parse_keys("ctrl+ctrl+s").unwrap(), vec!["ctrl", "s"]);
    }

    #[test]
    fn every_allowed_key_actually_parses() {
        // A key in the list that the parser rejects would be a list that lies.
        for k in ALLOWED_KEYS {
            assert!(
                parse_keys(k).is_ok(),
                "{k} is allow-listed but does not parse"
            );
        }
    }

    /* ── bounds ──────────────────────────────────────────────────── */

    #[test]
    fn the_scroll_amount_is_bounded_both_ways() {
        assert_eq!(1000i32.clamp(-MAX_SCROLL, MAX_SCROLL), MAX_SCROLL);
        assert_eq!((-1000i32).clamp(-MAX_SCROLL, MAX_SCROLL), -MAX_SCROLL);
        assert_eq!(3i32.clamp(-MAX_SCROLL, MAX_SCROLL), 3);
    }

    #[test]
    fn typing_length_is_bounded() {
        let long = "a".repeat(MAX_TYPE_CHARS + 500);
        assert!(long.chars().count() > MAX_TYPE_CHARS);
        let cut: String = long.chars().take(MAX_TYPE_CHARS).collect();
        assert_eq!(cut.chars().count(), MAX_TYPE_CHARS);
    }
}
