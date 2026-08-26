// Hide the console window on Windows release builds. Compass is a window, not a
// command, and a black console flashing up behind it looks like a fault.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    compass_desktop_lib::run();
}
