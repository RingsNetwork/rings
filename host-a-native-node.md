# Host a Native Node

### Prepare your configuration

After installation, you can create your configuration by:

```bash
rings init
```

This command will generate a configuration file named config.yaml for you. The default path is $HOME/.rings/config.yaml. You can also customize the path by using `rings init --location <path>`. Additionally, you can specify the key used by the Rings node by using `rings init -k <your private key>`.

By default, an ECDSA key pair will be automatically generated for you. The key pair in Rings Network is used for user identification and message signing. The key pair is utilized in various components of the network, so it is essential for users to ensure the security of their key pair. In the Native Node environment, since the private key is stored in plaintext, we strongly **discourage** the use of any asset-related key pair to host Rings Network.

More about `config.yaml`you can find at:

[config.yaml.md](advanced-topic/config.yaml.md "mention")

## Ready go!

Now you can start your rings-node by using the following command:

```bash
rings run
```

You can use `rings run --help` to check which settings are supported by the `run` command.

```bash
# rings run --help
Starts a long-running node daemon.

Usage: rings run [OPTIONS]

Options:
  -b, --http-addr <HTTP_ADDR>
          Rings node listen address. If not provided, use bind_addr in config file or 127.0.0.1:50000 [env: HTTP_ADDR=]
  -s, --ice-servers <ICE_SERVERS>
          ICE server list. If not provided, use ice_servers in config file or stun://stun.l.google.com:19302 [env: ICE_SERVERS=]
  -k, --key <ECDSA_KEY>
          Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file [env: ECDSA_KEY=46886194468bb6e0faa36c12cebb6f0ca104ddbc8ec9d39246d718eba6e22d69]
      --stabilize-timeout <STABILIZE_TIMEOUT>
          Stabilize service timeout. If not provided, use stabilize_timeout in config file or 3 [env: STABILIZE_TIMEOUT=]
      --external-ip <EXTERNAL_IP>
          external ip address [env: EXTERNAL_IP=]
      --storage-path <STORAGE_PATH>
          Storage files location. If not provided, use storage.path in config file or ~/.local/share/rings [env: STORAGE_PATH=]
      --storage-capacity <STORAGE_CAPACITY>
          Storage capcity. If not provider, use storage.capacity in config file or 200000000 [env: STORAGE_CAPACITY=] [default: 200000000]
  -c, --config <CONFIG>
          Config file location [env: CONFIG=] [default: ~/.rings/config.yaml]
  -h, --help
          Print help
```



With the `rings run` command, you can override the configurations specified in the `config.yaml` file using command-line arguments. Additionally, you can use the `-c` parameter to specify a different config file, which is particularly useful when you need to run multiple instances of the Rings Network.





