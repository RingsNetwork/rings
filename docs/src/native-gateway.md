# Native Gateway

A native node can forward explicitly selected IPv4/TCP destinations through Rings Onion
circuits over a TUN device. The gateway is configured by the `gateway:` section of
`config.yaml`, which `rings init` writes in full with `enabled: false`; see
[config.yaml](advanced-topic/config.yaml.md#gateway) for the generated section.

Presence of the section is not consent to start a TUN device:

```text
gateway starts ⟺ section present ∧ (enabled = true ∨ rings run --gateway)
```

A section that omits `enabled` is inert, so plain `rings run` on the generated file creates no
interface. To run the gateway once without editing the file:

```bash
rings run --gateway
```

On a config written before the section existed, `--gateway` fails and prints the exact section
to append. The generated plan assigns one host address from the RFC 6598 shared address space
(`100.64.0.1/32`) and captures no destination, so enabling it creates the interface without
steering traffic until you list a prefix in `included_routes`. A TUN device also needs
`CAP_NET_ADMIN` on Linux or the foreground helper described below; the configuration grants
neither.

The rest of this page is the operator guide from the `rings-gateway` crate.

{{#include ../../crates/gateway/README.md:operator-guide}}
