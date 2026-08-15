#ifndef RINGS_NODE_H
#define RINGS_NODE_H

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
 * Craft a new Provider with signer.
 *
 * # Safety
 *
 * String pointers must be valid null-terminated UTF-8 strings. The signer must
 * write exactly 65 signature bytes into the provided output buffer. Returns NULL
 * when provider creation fails. The returned handle must be released exactly
 * once with `rings_node_provider_destroy`.
 */
struct ProviderHandle *rings_node_new_provider_with_callback(uint32_t network_id,
                                                             const char *ice_server,
                                                             uint64_t stabilize_interval,
                                                             const char *account,
                                                             const char *account_type,
                                                             void (*signer)(const char *, char *));

#endif
