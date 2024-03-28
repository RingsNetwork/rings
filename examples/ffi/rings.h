typedef enum LogLevel {
  Debug,
  Info,
  Warn,
  Error,
  Trace,
} LogLevel;

/**
 * Processor for rings-node rpc server
 */
typedef struct Processor Processor;

/**
 * Internal rpc handler for rings-node rpc server
 */
typedef struct InternalRpcHandler InternalRpcHandler;

typedef struct InternalRpcHandler InternalRpcHandler;

/**
 * A wrapper for FFIbackendbehaviour, we needs runtime to make async request work
 */
typedef struct FFIBackendBehaviourWithRuntime FFIBackendBehaviourWithRuntime;


/**
 * A structure to represent the Provider in a C-compatible format.
 * This is necessary as using Arc directly in FFI can be unsafe.
 */
typedef struct ProviderPtr {
  const struct Processor *processor;
  const InternalRpcHandler *handler;
} ProviderPtr;

/**
 * Context for handling backend behaviour
 */
typedef struct FFIBackendBehaviour {
  void (**paintext_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                    const struct ProviderPtr*,
                                    const char*,
                                    const char*);
  void (**service_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                   const struct ProviderPtr*,
                                   const char*,
                                   const char*);
  void (**extension_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                     const struct ProviderPtr*,
                                     const char*,
                                     const char*);
} FFIBackendBehaviour;

/**
 * Backend behaviour for FFI
 */
struct FFIBackendBehaviour new_ffi_backend_behaviour(void (*paintext_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                                                                      const struct ProviderPtr*,
                                                                                      const char*,
                                                                                      const char*),
                                                     void (*service_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                                                                     const struct ProviderPtr*,
                                                                                     const char*,
                                                                                     const char*),
                                                     void (*extension_message_handler)(const struct FFIBackendBehaviourWithRuntime*,
                                                                                       const struct ProviderPtr*,
                                                                                       const char*,
                                                                                       const char*));

void init_logging(enum LogLevel level);

/**
 * Start message listening and stabilization
 * # Safety
 * Listen function accept a ProviderPtr and will unsafety cast it into Arc based Provider
 */
void listen(const struct ProviderPtr *provider_ptr);

/**
 * Request internal rpc api
 * # Safety
 *
 * * This function accept a ProviderPtr and will unsafety cast it into Arc based Provider
 * * This function cast CStr into Str
 */
const char *request(const struct ProviderPtr *provider_ptr, const char *method, const char *params);

/**
 * Craft a new Provider with signer and callback ptr
 * # Safety
 *
 * * This function cast CStr into Str
 */
struct ProviderPtr new_provider_with_callback(const char *ice_server,
                                              uint64_t stabilize_interval,
                                              const char *account,
                                              const char *account_type,
                                              void (*signer)(const char*, char*),
                                              const struct FFIBackendBehaviour *callback_ptr);
