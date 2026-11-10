#ifndef RINGS_NODE_H
#define RINGS_NODE_H

#include <stddef.h>
#include <stdint.h>

typedef enum LogLevel {
  Debug,
  Info,
  Warn,
  Error,
  Trace,
} LogLevel;

typedef struct ProviderHandle ProviderHandle;

void rings_node_init_logging(enum LogLevel level);

/**
 * Start message listening and stabilization.
 *
 * # Safety
 *
 * `provider_ptr` must be NULL or a live ProviderHandle returned by
 * `rings_node_new_provider_with_callback`. NULL is logged and ignored. Using a
 * handle after `rings_node_provider_destroy` is undefined behavior.
 */
void rings_node_listen(const struct ProviderHandle *provider_ptr);

/**
 * Request internal rpc api.
 *
 * # Safety
 *
 * `provider_ptr` must be NULL or a live ProviderHandle returned by
 * `rings_node_new_provider_with_callback`; `method` and `params` must be valid
 * null-terminated UTF-8 strings. Returns NULL when request validation or
 * execution fails. Every non-NULL returned string must be released exactly once
 * with `rings_node_string_free`.
 */
char *rings_node_request(const struct ProviderHandle *provider_ptr,
                         const char *method,
                         const char *params);

/**
 * Free a string returned by `rings_node_request`.
 *
 * Passing NULL is a no-op. Passing any pointer not returned by
 * `rings_node_request`, or freeing the same pointer twice, is undefined
 * behavior.
 */
void rings_node_string_free(char *value);

/**
 * Destroy a ProviderHandle returned by `rings_node_new_provider_with_callback`.
 *
 * Passing NULL is a no-op. The pointer is invalid after this call. Do not call
 * concurrently with other functions that use the same handle.
 */
void rings_node_provider_destroy(struct ProviderHandle *provider_ptr);

/**
 * Craft a new Provider with signer, keeping its stores under `storage_path`.
 *
 * `storage_path` is a directory the provider owns: it opens the node's stores
 * under it as the native daemon does (the DHT data store, the authoritative
 * transaction replay store, the retained relay inbox, the onion entry guards
 * and the measurement stores). Reusing the path across runs keeps the replay
 * windows, held mail and entry guards; a fresh path is a fresh node state.
 *
 * The provider owns its stores exclusively while it lives: each store locks
 * its directory at open (#1039), so a second provider, or a native daemon,
 * given a store this provider holds fails to open it, and creation returns
 * NULL, instead of interleaving writes with it. The lock is per store:
 * providers with distinct roots never conflict.
 *
 * # Safety
 *
 * String pointers, `storage_path` included, must be valid null-terminated
 * UTF-8 strings, and `signer` must be non-NULL. `signer` receives `(proof,
 * out, out_cap)` where `out_cap` is the byte capacity of `out` (96 bytes, the
 * widest supported signature). It must write at most `out_cap` signature bytes
 * matching `account_type` (65 for `eip191`/`secp256k1`/`bip137`, 64 for
 * `ed25519`/`secp256r1`, 96 for `bls12-381`) into `out` and return the number
 * of bytes written, or `0` on failure. Returns NULL when provider creation
 * fails, a store that cannot be opened or an invalid signature length
 * included. The returned handle must be released exactly once with
 * `rings_node_provider_destroy`.
 */
struct ProviderHandle *rings_node_new_provider_with_callback(uint32_t network_id,
                                                             const char *ice_server,
                                                             uint64_t stabilize_interval,
                                                             const char *account,
                                                             const char *account_type,
                                                             const char *storage_path,
                                                             size_t (*signer)(const char *, char *, size_t));

#endif
