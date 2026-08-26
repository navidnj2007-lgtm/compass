//! Facts about the machine.
//!
//! Read-only and low risk, but not unbounded: this returns the shape of the
//! computer, not an inventory of it. No process list, no network interfaces, no
//! installed-software enumeration. Those are the things that make a fingerprint
//! worth exfiltrating, and none of them help the assistant plan a study week or
//! tidy a folder, which is what it is for.

use crate::agent::{Agent, ToolOut};
use serde::Deserialize;
use sysinfo::{Disks, System};
use tauri::State;

#[derive(Debug, Deserialize)]
pub struct SystemInfoReq {}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn hhmm(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min")
    }
}

#[tauri::command]
pub async fn get_system_information(
    state: State<'_, Agent>,
    req: SystemInfoReq,
) -> Result<ToolOut, String> {
    let _ = req;
    let out = collect();
    state.audit.record(
        "win.get_system_information",
        true,
        "Read system information".to_string(),
        None,
        false,
    );
    Ok(out)
}

fn collect() -> ToolOut {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_all();

    let mut text = String::from("THIS COMPUTER\n");

    text.push_str(&format!(
        "  System: {} {}\n",
        System::name().unwrap_or_else(|| "Windows".into()),
        System::os_version().unwrap_or_default()
    ));
    if let Some(k) = System::kernel_version() {
        text.push_str(&format!("  Build: {k}\n"));
    }

    let cpus = sys.cpus();
    if let Some(first) = cpus.first() {
        text.push_str(&format!(
            "  Processor: {} ({} cores)\n",
            first.brand().trim(),
            cpus.len()
        ));
    }

    let total = sys.total_memory();
    let used = sys.used_memory();
    if total > 0 {
        text.push_str(&format!(
            "  Memory: {} of {} in use ({:.0}%)\n",
            gb(used),
            gb(total),
            (used as f64 / total as f64) * 100.0
        ));
    }

    text.push_str(&format!("  Uptime: {}\n", hhmm(System::uptime())));

    let disks = Disks::new_with_refreshed_list();
    if !disks.is_empty() {
        text.push_str("  Disks:\n");
        for d in disks.iter() {
            let total = d.total_space();
            if total == 0 {
                continue;
            }
            let free = d.available_space();
            text.push_str(&format!(
                "    {} — {} free of {}\n",
                d.mount_point().display(),
                gb(free),
                gb(total)
            ));
        }
    }

    ToolOut::text(text)
}
