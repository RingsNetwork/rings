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

typedef struct ProviderPtr {
  const void *provider;
  const void *runtime;
} ProviderPtr;

void rings_node_init_logging(enum LogLevel level);

/**
 * Start message listening and stabilization.
 *
 * # Safety
 *
 * `provider_ptr` must point to a ProviderPtr returned by
 * `rings_node_new_provider_with_callback`. Invalid provider pointers are logged and
 * ignored.
 */
void rings_node_listen(const struct ProviderPtr *provider_ptr);

/**
 * Request internal rpc api.
 *
 * # Safety
 *
 * `provider_ptr` must point to a ProviderPtr returned by
 * `rings_node_new_provider_with_callback`; `method` and `params` must be valid
 * null-terminated UTF-8 strings. Returns NULL when request validation or
 * execution fails.
 */
const char *rings_node_request(const struct ProviderPtr *provider_ptr,
                               const char *method,
                               const char *params);

/**
 * Craft a new Provider with signer.
 *
 * # Safety
 *
 * String pointers must be valid null-terminated UTF-8 strings. The signer must
 * write exactly 65 signature bytes into the provided output buffer. Returns a
 * ProviderPtr with NULL fields when provider creation fails.
 */
struct ProviderPtr rings_node_new_provider_with_callback(uint32_t network_id,
                                                         const char *ice_server,
                                                         uint64_t stabilize_interval,
                                                         const char *account,
                                                         const char *account_type,
                                                         void (*signer)(const char *, char *));

#endif
