
## Decentralized Service Examples s

### Run a simple http json service

We use [json-server](https://github.com/typicode/json-server) as the example http service provider.

1. Install `json-server`

```
npm install -g json-server
```

2. Define the data

Create a `db.json` and write the data into the file:

```json
{
    "posts": [
        {
            "id": 1,
            "author": "Rings Network de-service",
            "content": "Decentralize the world!"
        }
    ]
}
```

3. Run the `json-server`

```
json-server --watch db.json --port 8000 --host 0.0.0.0
```

4. Test the server response

You can use `curl` or just visit `http://127.0.0.1:8000/posts` in browsers to check if the `json-server` works fine.

### Run a rings-node(name:node-0) and register the service

1. Install the lastest Rings node

Let's name this node `node-0`

```
cargo install rings-node
```

> You can visit [install-a-native-node.md](../install-a-native-node.md) for more installation details.

2. Create a config file for `node-0`:

```
rings init --location ./node0-config.yaml
```

Edit the config file to register the service:

```
backend:
- name: sample_json_server
  register_service: sample_json_server
  prefix: http://127.0.0.1:8000
```

The whole config file would be something like this:

```
bind: 127.0.0.1:50000
endpoint_url: http://127.0.0.1:50000
ecdsa_key: <your-node0-private-key>
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 20
external_ip: null
data_storage:
  path: <your-data-dir>
  capacity: 200000000
measure_storage:
  path: <your-mesure-dir>
  capacity: 200000000
backend:
- name: sample_json_server
  register_service: sample_json_server
  prefix: http://127.0.0.1:8000
```

3. Run node-0

```
rings run --config ./node0-config.yaml
```

### Run another rings-node(name:node-1) and request the service

1. Create a config file for `node-1`:

```
rings init --location ./node1-config.yaml
```

The whole config file would be something like this:

```
bind: 127.0.0.1:50001
endpoint_url: http://127.0.0.1:50001
ecdsa_key: <your-node0-private-key>
ice_servers: stun://stun.l.google.com:19302
stabilize_timeout: 20
external_ip: null
data_storage:
  path: <your-data-dir>
  capacity: 200000000
measure_storage:
  path: <your-mesure-dir>
  capacity: 200000000
```

2. Run node-1

```
rings run --config ./node1-config.yaml
```

### Connect node-0 and node-1 together

You can connect `node-0` to `node-1` or per se to build a network. Or they can all connect to an existing network.

Here we connect `node-0` to `node-1`:

```
rings connect node --config ./node1-config.yaml http://127.0.0.1:50001
```

Then we check if two nodes are connected together:

```
rings peer list --config ./node0-config.yaml
```

Result:
```
Did, TransportId, Status
e9ef508489695250e1af81cc4cc376a9109b68af, 4e36b64e-7292-4d9c-89b8-9d05c735301e, connected
```

It's connected now.

### Make the request

```
rings send http --config ./node1-config.yaml --method GET <node0-did> sample_json_server /posts
```

It prints `Done.` if nothing wrong.

### Check the response

Let's check using [polling message api](../jsonrpc.md#pollmessage):

```
curl -X POST -H "Content-Type: application/json" --data '{"jsonrpc": "2.0", "id": 1, "method": "pollMessage", "params": ["WAIT"]}' "http://127.0.0.1:50001"
```

Result:
```json
{"jsonrpc":"2.0","result":{"message":{"data":"H4sIAAAAAAAC/2VRW0tjMRB2L6y7ZXcfd5/E8Tw32ItVqfigWERBBakKig8xZ3pObJqEZE4viv/Pn2WqiYoGQr75MtdvHhf+LLyc3/EVRhNqYjSzuBw5bq2SgpM0evXWG70FouTOI21XNGCbfz+EKtQFld8i22xsfI8QiRdZxBer2foN6zc2i0vFz6jte46THlW83D05H/c6B1mKyjnhUsT9CuvQ6sBhpaDVaLWh2e62W921Ndg/6i+mMlMrHfqv0WTNWkRDRMu4kmP8FRmSIzQVbXdSsTF3s/8RnzhZSF2HHSHQEutpYXKpC0iqBNp7Np/aGRXyKjNhwmEeNJBc+ZSSXIVJZcFFiSnkZyS1Yc/8j2hbx4sR//hbe1NZo5jv4vNYaYlTZs0EQyvsZpZU6U1tEMX/e3V5v2lm7DyjX3wt6rUcDL4Mo31VA7gPFyCTedaFZv3F4BWVxgUiOw3KeDhGmhg3hByZRzeWArPoGavNXfdQBORCx3cIVCKEEJWvZMHxoXb9BECeCOCTAgAA","message_type":4}},"id":1}
```

> MessageType:
>  ```
>  pub enum MessageType {
>      /// unknown
>      Unknown = 0,
>      /// empty
>      Empty,
>      /// simple texte
>      SimpleText,
>      /// http request
>      HttpRequest,
>      /// http response
>      HttpResponse,
>      /// extension
>      Extension,
>  }
>  ```

So `message_type` 4 means `HttpResponse`. Let's decode the data with base64 decoding and gunzip:

```
echo 'H4sIAAAAAAAC/2VRW0tjMRB2L6y7ZXcfd5/E8Tw32ItVqfigWERBBakKig8xZ3pObJqEZE4viv/Pn2WqiYoGQr75MtdvHhf+LLyc3/EVRhNqYjSzuBw5bq2SgpM0evXWG70FouTOI21XNGCbfz+EKtQFld8i22xsfI8QiRdZxBer2foN6zc2i0vFz6jte46THlW83D05H/c6B1mKyjnhUsT9CuvQ6sBhpaDVaLWh2e62W921Ndg/6i+mMlMrHfqv0WTNWkRDRMu4kmP8FRmSIzQVbXdSsTF3s/8RnzhZSF2HHSHQEutpYXKpC0iqBNp7Np/aGRXyKjNhwmEeNJBc+ZSSXIVJZcFFiSnkZyS1Yc/8j2hbx4sR//hbe1NZo5jv4vNYaYlTZs0EQyvsZpZU6U1tEMX/e3V5v2lm7DyjX3wt6rUcDL4Mo31VA7gPFyCTedaFZv3F4BWVxgUiOw3KeDhGmhg3hByZRzeWArPoGavNXfdQBORCx3cIVCKEEJWvZMHxoXb9BECeCOCTAgAA' | base64 -d | gunzip
```

It prints something like this:

```
�
content-typeapplication/json; charset=utf-8content-length107etag"W/"6b-T08gZlaUt3sEratnmuahBOVvE5I"dateTue, 25 Jul 2023 13:32:44 GMTexpires-1
cache-controno-cachepragmno-cachein, Accept-Encoding access-control-allow-credentialstrue
connection
keep-alive
          x-powered-byExpressx-content-type-optionsnosniffk[
  {
    "id": 1,
    "author": "Rings Network de-service",
    "content": "Decentralize the world!"
  }
]
```

It's in http response format.