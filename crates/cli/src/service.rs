#[cfg(any(target_os = "macos", test))]
mod macos;

#[cfg(not(target_os = "macos"))]
use std::path::Path;

use anyhow::Result;
#[cfg(not(target_os = "macos"))]
use anyhow::bail;
#[cfg(any(target_os = "macos", test))]
pub(crate) use macos::*;

use crate::args::ServiceArgs;

/// Dispatches one service lifecycle command.
pub(crate) fn run_service(args: ServiceArgs) -> Result<()> {
    run_platform_service(args)
}

/// Reports unsupported automatic background refresh on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_refresh_auto(_root: &Path) -> Result<()> {
    bail!("`darc refresh --auto` is currently supported only on macOS")
}

/// Reports unsupported service management on non-macOS platforms.
#[cfg(not(target_os = "macos"))]
pub(crate) fn run_platform_service(_args: ServiceArgs) -> Result<()> {
    bail!("`darc service` is currently supported only on macOS")
}
