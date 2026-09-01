# Rings dependency patch

This directory mirrors `netlink-packet-core 0.8.2` from crates.io. The only code-graph
change is in `Cargo.toml`: the dependency imported as `paste` resolves to the maintained,
API-compatible `pastey` fork. Keeping the package at 0.8.2 preserves the buffer macros used
by the currently selected `netlink-packet-route` versions.

Remove this patch after a compatible upstream release no longer depends on archived `paste`.
