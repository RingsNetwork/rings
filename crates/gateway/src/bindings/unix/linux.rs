//! Linux `/dev/net/tun` builder parameters.

use tun_rs::DeviceBuilder;

use super::super::NativeTunnelOptions;

/// Apply Linux `/dev/net/tun` options to a `tun_rs::DeviceBuilder`.
pub(crate) fn configure(
    mut builder: DeviceBuilder,
    options: &NativeTunnelOptions,
) -> DeviceBuilder {
    if let Some(name) = &options.interface_name {
        builder = builder.name(name.clone());
    }
    builder.with(|platform| {
        platform
            .packet_information(false)
            .offload(false)
            .multi_queue(false);
    })
}
