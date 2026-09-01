//! Platform-neutral route types used by the native tunnel transaction.

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use route_manager::Route;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use route_manager::RouteManager;

#[cfg(target_os = "windows")]
pub(crate) use super::windows::Route;
#[cfg(target_os = "windows")]
pub(crate) use super::windows::RouteManager;
