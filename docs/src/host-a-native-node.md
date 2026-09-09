# Host a Native Node

### Prepare your configuration

After installation, you can create your configuration by:

```bash
rings init
```

This command will generate a configuration file named config.yaml for you. The default path is $HOME/.rings/config.yaml. You can also customize the path by using `rings init --location <path>`. Additionally, you can specify the key used by the Rings node by using `rings init -k <your private key>`. The generated file states every section explicitly, including a complete `gateway:` section with `enabled: false`, so it is the one place you edit; see [Native Gateway](native-gateway.md).

By default, an ECDSA key pair will be automatically generated for you. The key pair in Rings Network is used for user identification and message signing. The key pair is utilized in various components of the network, so it is essential for users to ensure the security of their key pair. In the Native Node environment, since the private key is stored in plaintext, we strongly **discourage** the use of any asset-related key pair to host Rings Network.

More about `config.yaml`you can find at:

[config.yaml.md](advanced-topic/config.yaml.md)

## Ready go!

Now you can start your rings-node by using the following command:

```bash
rings run
```

You can use `rings run --help` to check which settings are supported by the `run` command.

```text
# rings run --help
Runs a foreground, composable Rings node.

Usage: rings run [OPTIONS]

Options:
      --gateway
          Start the native TUN gateway from the config's gateway section for this run; the section alone never starts it (rings init writes it with enabled: false) [env: GATEWAY=]
      --external-api-addr <EXTERNAL_API_ADDR>
          Rings node external api listen address. If not provided, use external_api_addr in config file or 127.0.0.1:50001 [env: EXTERNAL_API_ADDR=]
      --internal-api-port <INTERNAL_API_PORT>
          Rings node internal api listen port. If not provided, use internal_api_port in config file or 50000
      --api-token-path <API_TOKEN_PATH>
          API Bearer token file guarding the internal API and the external status and registry reads; the external handshake (nodeDid, answerOffer) is public. Relative paths are resolved next to the node config file [env: API_TOKEN_PATH=]
      --api-allowed-origin <API_ALLOWED_ORIGINS>
          Exact browser origin permitted to call the authenticated API; repeat as needed [env: API_ALLOWED_ORIGINS=]
      --allow-remote-external-api
          Explicitly permit external_api_addr to bind a non-loopback address [env: ALLOW_REMOTE_EXTERNAL_API=]
      --ice-servers <ICE_SERVERS>
          ICE server list. If not provided, use ice_servers in config file or stun://stun.l.google.com:19302 [env: ICE_SERVERS=]
  -k, --key <ECDSA_KEY>
          Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file [env: ECDSA_KEY=]
      --stabilize-interval <STABILIZE_INTERVAL>
          Stabilization interval in seconds. If not provided, use stabilize_interval in config file or 15 [env: STABILIZE_INTERVAL=]
      --external-ip <EXTERNAL_IP>
          external ip address [env: EXTERNAL_IP=]
      --webrtc-udp-port-min <WEBRTC_UDP_PORT_MIN>
          Minimum UDP port used by native WebRTC ICE gathering. Must be paired with --webrtc-udp-port-max. [env: WEBRTC_UDP_PORT_MIN=]
      --webrtc-udp-port-max <WEBRTC_UDP_PORT_MAX>
          Maximum UDP port used by native WebRTC ICE gathering. Must be paired with --webrtc-udp-port-min. [env: WEBRTC_UDP_PORT_MAX=]
      --storage-path <STORAGE_PATH>
          Storage files location. If not provided, use storage.path in config file or ~/.local/share/rings [env: STORAGE_PATH=]
      --storage-capacity <STORAGE_CAPACITY>
          Storage capacity. If not provider, use storage.capacity in config file or 200000000 [env: STORAGE_CAPACITY=] [default: 200000000]
      --reassembly-profile <REASSEMBLY_PROFILE>
          Inbound chunk reassembly memory profile [env: REASSEMBLY_PROFILE=] [default: production] [possible values: production, constrained]
      --advertise-onion-relay
          Advertise this node as an onion relay in the online-node registry [env: ADVERTISE_ONION_RELAY=]
      --advertise-onion-exit
          Publish this node as an onion exit in the application-layer exit registry [env: ADVERTISE_ONION_EXIT=]
      --onion-exit-service <ONION_EXIT_SERVICE>
          Exit service in name:transport form, e.g. https:tcp or web:tcp. May be repeated. [env: ONION_EXIT_SERVICE=]
      --onion-exit-allow-target <ONION_EXIT_ALLOW_TARGET>
          Allow-list target for onion exit policy. May be repeated. [env: ONION_EXIT_ALLOW_TARGET=]
      --onion-exit-deny-target <ONION_EXIT_DENY_TARGET>
          Deny-list target for onion exit policy. May be repeated. [env: ONION_EXIT_DENY_TARGET=]
      --onion-exit-max-circuits <ONION_EXIT_MAX_CIRCUITS>
          Maximum onion circuits this exit will serve [env: ONION_EXIT_MAX_CIRCUITS=]
      --onion-exit-max-streams-per-circuit <ONION_EXIT_MAX_STREAMS_PER_CIRCUIT>
          Maximum streams per onion circuit this exit will serve [env: ONION_EXIT_MAX_STREAMS_PER_CIRCUIT=]
      --onion-exit-max-bytes-per-minute <ONION_EXIT_MAX_BYTES_PER_MINUTE>
          Maximum bytes per minute this exit will serve [env: ONION_EXIT_MAX_BYTES_PER_MINUTE=]
      --onion-exit-heartbeat-interval-secs <ONION_EXIT_HEARTBEAT_INTERVAL_SECS>
          Onion-exit registry heartbeat interval in seconds [env: ONION_EXIT_HEARTBEAT_INTERVAL_SECS=]
      --onion-exit-ttl-secs <ONION_EXIT_TTL_SECS>
          Onion-exit registry descriptor TTL in seconds [env: ONION_EXIT_TTL_SECS=]
      --onion-http-proxy-addr <ONION_HTTP_PROXY_ADDR>
          Bind a local HTTP CONNECT proxy that routes client TCP streams through onion exits, e.g. 127.0.0.1:18080 [env: ONION_HTTP_PROXY_ADDR=]
      --onion-http-proxy-service <ONION_HTTP_PROXY_SERVICE>
          TCP onion-exit service used by the local HTTP CONNECT proxy, e.g. tcp or web [env: ONION_HTTP_PROXY_SERVICE=]
      --onion-http-proxy-hop-count <ONION_HTTP_PROXY_HOP_COUNT>
          Desired hop count for the local onion HTTP proxy. 0 uses node default. [env: ONION_HTTP_PROXY_HOP_COUNT=]
      --onion-http-proxy-allow-short-paths
          Allow the local onion HTTP proxy to use shorter routes when too few relays are live [env: ONION_HTTP_PROXY_ALLOW_SHORT_PATHS=]
      --onion-http-proxy-header-timeout-secs <ONION_HTTP_PROXY_HEADER_TIMEOUT_SECS>
          Maximum seconds to wait for one HTTP CONNECT header [env: ONION_HTTP_PROXY_HEADER_TIMEOUT_SECS=]
      --onion-http-proxy-max-connections <ONION_HTTP_PROXY_MAX_CONNECTIONS>
          Maximum concurrent local HTTP CONNECT proxy connections [env: ONION_HTTP_PROXY_MAX_CONNECTIONS=]
  -c, --config <CONFIG>
          Config file location [env: CONFIG=] [default: ~/.rings/config.yaml]
  -h, --help
          Print help
```



With the `rings run` command, you can override the configurations specified in the `config.yaml` file using command-line arguments. Additionally, you can use the `-c` parameter to specify a different config file, which is particularly useful when you need to run multiple instances of the Rings Network.

## Enable the native gateway

The generated `gateway:` section is disabled, so `rings run` creates no TUN device. To start the
gateway for one run without editing the file:

```bash
rings run --gateway
```

To start it on every run, set `enabled: true` in the section. Either way the interface needs
`CAP_NET_ADMIN` on Linux or the foreground helper; see [Native Gateway](native-gateway.md).
