//! Windows Wintun builder parameters.

use tun_rs::DeviceBuilder;

use super::NativeTunnelOptions;

mod route;

pub(crate) use route::Route;
pub(crate) use route::RouteManager;

pub(crate) fn configure(
    mut builder: DeviceBuilder,
    options: &NativeTunnelOptions,
) -> DeviceBuilder {
    if let Some(name) = &options.interface_name {
        builder = builder.name(name.clone());
    }
    builder.with(|platform| {
        platform
            .description("Rings IPv4/TCP Gateway")
            .wintun_log(false)
            .ring_capacity(4 * 1_024 * 1_024);
        if let Some(path) = &options.wintun_dll_path {
            platform.wintun_file(path.to_string_lossy().into_owned());
        }
    })
}
