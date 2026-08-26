/* Declaring the app's own commands here makes them ACL-checked like plugin
commands, instead of implicitly available to every webview. That matters
because this app deliberately loads a remote frontend: the capability files
then decide, explicitly and reviewably, which commands that remote origin may
reach. Anything not on this list cannot be invoked over IPC at all.

It is defence in depth rather than the defence itself — Tauri has had both an
iframe IPC bypass (GHSA-57fm-592m-34r7) and an origin confusion bug
(GHSA-7gmj-67g7-phm9), so the real enforcement lives inside each command. */
fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "agent_handshake",
            "agent_audit",
            "agent_open_settings",
            "pc_grant",
            "pc_revoke",
            "pc_grant_state",
            "pc_panic_armed",
            "list_files",
            "search_files",
            "grep_files",
            "read_document",
            "diff_file",
            "restore_file",
            "read_file",
            "get_system_information",
            "clipboard_read",
            "create_folder",
            "write_file",
            "move_file",
            "rename_file",
            "delete_file",
            "open_application",
            "open_url",
            "browser_tabs",
            "browser_open",
            "browser_read",
            "browser_click",
            "browser_type",
            "clipboard_write",
            "show_notification",
        ]),
    ))
    .expect("failed to run tauri-build");
}
