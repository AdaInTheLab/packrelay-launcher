// Shared launcher logic, lifted out of the original single-binary
// crate when we added a Tauri GUI. Both the CLI (crates/packrelay-
// cli) and the GUI (crates/packrelay-app, when it lands) depend on
// this crate — install + verify code is written exactly once and
// reused.

pub mod blob_cache;
pub mod client;
pub mod install;
pub mod manifest;
pub mod profile;
pub mod uninstall;
pub mod update;
pub mod verify;
