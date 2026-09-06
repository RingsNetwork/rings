# Decentralized services

<figure><img src="../.gitbook/assets/Elder_Ryan_webassembly_p2p_network_627824ef-f0b8-4887-ab7b-c166dac4b4ad.png" alt=""><figcaption></figcaption></figure>

Rings Network enables users to integrate services into its decentralized network. Its underlying mechanism relies on Rings DHT for service registration and discovery. Setting up hidden services is a straightforward process that decentralizes any service with just a few simple steps.

## Understand de-service

For classic Internet architecture, "services" typically refer to centralized services and entities. However, by using a Rings network, it is possible to effectively transform those centralized services into decentralized services.

This is achieved through a Rings custom backend. By configuring the Rings network to act as a transparent forwarder, all messages directed towards a registered service will ultimately be processed by this custom backend handler.

### Register of de-service

Every de-service provider can register itself in the network. The Rings network will create a key-value pair for each service in the form of `hash(services_name): [services_provider_did]` and store it using the DHT (Distributed Hash Table) feature across the network. When other network nodes request a de-service, this traffic will be forwarded to the corresponding active node providing that service.

In fact, we will utilize the Chord algorithm to find the corresponding data based on the hash of the register\_name. For more details, you can continue reading the section about the Distributed Hash Table (DHT).

[chord.md](../advanced-topic/chord.md "mention")

### Lookup of de-service

De-services can declare themselves as "alive" by using polling. This will help them maintain a relatively higher position in the de-service provider list. When a requester needs to request a de-service, it should first use the "service lookup" command to find the nodes that provide the required service:

```
rings service lookup --help
Usage: rings service lookup [OPTIONS] <NAME>

Arguments:
  <NAME>

Options:
  -u, --endpoint-url <ENDPOINT_URL>  rings-node endpoint url. If not provided, use endpoint_url in config file or http://127.0.0.1:50000 [env: ENDPOINT_URL=]
  -k, --key <ECDSA_KEY>              Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file [env: ECDSA_KEY=]
  -c, --config <CONFIG>              Config file location [env: CONFIG=] [default: ~/.rings/config.yaml]
  -h, --help                         Print help
```

This command can also be accomplished through the JSON-RPC API.

```
## Replace YOUR-SIGNATURE with your signature
## Replace NAME with the service name what you want to lookup in rings network
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "lookupService", "params": ["NAME"]}' \
"http://127.0.0.1:50000"
```

It will return a provider list of de-service, and then you can use the elements in the list to request de-services.

### Request de-services

Rings CLI provides a set of tools to assist in requesting de-services. This can be accomplished through the command line or via our Curl API.

For command-line usage:

```
rings send http --help
Sends an HTTP request message.

Usage: rings send http [OPTIONS] <TO_DID> <NAME> [PATH]

Arguments:
  <TO_DID>
  <NAME>
  [PATH]    [default: /]

Options:
  -u, --endpoint-url <ENDPOINT_URL>  rings-node endpoint url. If not provided, use endpoint_url in config file or http://127.0.0.1:50000 [env: ENDPOINT_URL=]
  -k, --key <ECDSA_KEY>              Your ECDSA key. If not provided, use ECDSA_KEY in env or ecdsa_key in config file [env: ECDSA_KEY=]
  -c, --config <CONFIG>              Config file location [env: CONFIG=] [default: ~/.rings/config.yaml]
  -X, --method <METHOD>              request method [default: get]
  -H, --header <HEADERS>             headers append to the request
  -b, --body <BODY>                  set content of http body
      --timeout <TIMEOUT>            [default: 30000]
  -h, --help                         Print help
```

For curl (JSON-RPC) usage

```
## Replace YOUR-SIGNATURE with your signature
## Replace REMOTE-PEER-DID with did of remote peer
## Replace HTTP-REQUEST-ARG with your request arguments
curl -X POST \
-H "Content-Type: application/json" \
-H "X-SIGNATURE: YOUR-SIGNATURE" \
--data '{"jsonrpc": "2.0", "id": 1, "method": "sendSimpleTextMessage", "params": ["REMOTE-PEER-DID", {HTTP-REQUEST-ARG}]}' \
"http://127.0.0.1:50000"
```

## Backend

Backends can be classified into various types based on different transmission protocols, such as TCP-Backend, HTTP-Backend, and WASM-Backend. The essence of a backend is that it acts as a custom message handler. It must have a service name and a delegated service path.

For example, we can declare a backend service with the name "ipfs\_provider." This service will redirect all requests directed towards its name to a configured prefix handler, which, in our example, is [http://127.0.0.1:8080](http://127.0.0.1:8080).

Thus, our config.yaml can be looks like this:

### Config your backend

Rings allows users to configure hidden services through the "config.yaml" file. If you are not familiar with what "config.yaml" is, I recommend reading up on it.

[config.yaml.md](../advanced-topic/config.yaml.md "mention")

For a hidden service, three fields need to be provided: "name," "register\_service," and "prefix". For example:

```yaml
- name: ipfs
  register_service: ipfs_provider
  prefix: http://127.0.0.1:8080
```

This config will perform two tasks:

1. It will use the "register\_service" function to register itself as an "ipfs\_provider" within the Rings Network.
2. It will forward all incoming traffic from the Rings Network to the specified "prefix."

Now let's integrate it into the `config.yaml` file:

```bash
# cat ./config.yaml

bind: 127.0.0.1:50000
endpoint_url: http://127.0.0.1:50000
ecdsa_key: <your private key>
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 3
external_ip: null
backend:
- name: ipfs
  register_service: ipfs_provider
  prefix: http://127.0.0.1:8080
...
```

Once all the configurations are set up, you can use the command `rings run` to start hosting your decentralized services.

## An example of de-service

[decentralized-service-examples.md](../examples/decentralized-service-examples.md "mention")
