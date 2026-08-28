#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "../../../crates/node/include/rings.h"

struct ProviderHandle {
  void (*signer)(const char *, char *);
  char *account;
};

static char *copy_string(const char *value) {
  size_t length = strlen(value);
  char *copy = malloc(length + 1);
  if (copy != NULL) {
    memcpy(copy, value, length + 1);
  }
  return copy;
}

static const char *fixture_peer_did(const struct ProviderHandle *provider) {
  return strcmp(provider->account, "fixture-one") == 0 ? "fixture-two"
                                                       : "fixture-one";
}

void rings_node_init_logging(enum LogLevel level) { (void)level; }

void rings_node_listen(const struct ProviderHandle *provider) { (void)provider; }

char *rings_node_request(const struct ProviderHandle *provider,
                         const char *method,
                         const char *params) {
  (void)params;
  if (provider == NULL || method == NULL) {
    return NULL;
  }
  char response[512];
  int length;
  if (strcmp(method, "nodeInfo") == 0) {
    length = snprintf(response, sizeof(response), "{\"ok\":true}");
  } else if (strcmp(method, "nodeDid") == 0) {
    length = snprintf(response, sizeof(response), "{\"did\":\"%s\"}",
                      provider->account);
  } else if (strcmp(method, "createOffer") == 0) {
    length = snprintf(response, sizeof(response),
                      "{\"offer\":\"fixture-offer\"}");
  } else if (strcmp(method, "answerOffer") == 0) {
    length = snprintf(response, sizeof(response),
                      "{\"answer\":\"fixture-answer\"}");
  } else if (strcmp(method, "acceptAnswer") == 0) {
    length = snprintf(response, sizeof(response), "{\"accepted\":true}");
  } else if (strcmp(method, "listPeers") == 0) {
    length = snprintf(
        response, sizeof(response),
        "{\"peers\":[{\"did\":\"%s\",\"state\":\"Connected\"}]}",
        fixture_peer_did(provider));
  } else if (strcmp(method, "sendE2eHandshake") == 0) {
    length = snprintf(response, sizeof(response),
                      "{\"tx_id\":\"fixture-transaction\"}");
  } else if (strcmp(method, "sendE2eMessage") == 0) {
    length = snprintf(response, sizeof(response),
                      "{\"stream_id\":\"fixture-stream\"}");
  } else if (strcmp(method, "takeE2eEvents") == 0) {
    length = snprintf(
        response, sizeof(response),
        "{\"events\":[{\"kind\":\"streamFrame\",\"stream_id\":\"fixture-stream\",\"is_final\":true}]}");
  } else if (strcmp(method, "signerByte") == 0) {
    char signature[65];
    provider->signer("rings ffi fixture", signature);
    length = snprintf(response, sizeof(response), "{\"byte\":%u}",
                      (unsigned char)signature[0]);
  } else {
    return NULL;
  }
  if (length < 0 || (size_t)length >= sizeof(response)) {
    return NULL;
  }
  char *result = malloc((size_t)length + 1);
  if (result == NULL) {
    return NULL;
  }
  memcpy(result, response, (size_t)length + 1);
  return result;
}

void rings_node_string_free(char *value) { free(value); }

void rings_node_provider_destroy(struct ProviderHandle *provider) {
  if (provider == NULL) {
    return;
  }
  free(provider->account);
  free(provider);
}

struct ProviderHandle *rings_node_new_provider_with_callback(
    uint32_t network_id,
    const char *ice_server,
    uint64_t stabilize_interval,
    const char *account,
    const char *account_type,
    void (*signer)(const char *, char *)) {
  (void)network_id;
  (void)ice_server;
  (void)stabilize_interval;
  (void)account;
  (void)account_type;
  if (signer == NULL || account == NULL) {
    return NULL;
  }
  struct ProviderHandle *provider = malloc(sizeof(struct ProviderHandle));
  if (provider == NULL) {
    return NULL;
  }
  provider->signer = signer;
  provider->account = copy_string(account);
  if (provider->account == NULL) {
    free(provider);
    return NULL;
  }
  char signature[65];
  signer("rings ffi fixture", signature);
  return provider;
}
