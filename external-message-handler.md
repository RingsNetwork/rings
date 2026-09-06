# External message handler

## Endpoint



Rings Network supports implementing an external message handler using any programming language. This can be achieved by providing a WebSocket endpoint.&#x20;

You can implement a custom message handler by creating your own WebSocket service and listening on `<endpoint>/ws`. This allows you to handle custom messages according to your specific requirements.



## Extension

You can also extend the message handler by loading plugins, which is an advanced feature of the Rings Network. The supported extensions are primarily in the wasm format. Here's an example of a classic wasm plugin:

```
(module
  ;; Define a memory that is one page size (64kb)
  (memory (export "memory") 1)

  ;; fn handler(param: ExternRef) -> ExternRef
  (func $handler  (param externref) (result externref)
      (local.get 0)
      return
  )

  (export "handler" (func $handler))
)
```

In order to facilitate readability, we present the plugin in the WAT format (WebAssembly Text Format). You can generate it using any language, and it can actually run in any environment, such as a browser or native environment (similar to Rings).

We require the wasm plugin to include a function with the signature `Fn ExternRef -> ExternRef`, named "handler," which will be responsible for handling all requests targeting the extension.

Loading a wasm plugin is straightforward. You just need to insert the following into your config.yaml file.

```bash
extension:
  paths:
    - !Local ../example.wat
```

or

```bash
extension:
  paths:
    - !Remote https://ethereum.org/example.wasm
```

