#[cfg(feature = "browser")]
pub use browser_loader as loader;
#[cfg(not(feature = "browser"))]
pub use default_loader as loader;

use crate::backend::types::BackendMessage;

#[derive(Debug, Clone)]
pub struct BackendMessageRef {
    /// Message_type
    pub message_type: u16,
    /// extra bytes
    extra: Box<[u8]>,
    /// data body
    data: Box<[u8]>,
}

impl From<BackendMessage> for BackendMessageRef {
    fn from(msg: BackendMessage) -> Self {
        Self {
            message_type: msg.message_type,
            extra: msg.extra.into(),
            data: msg.data.into(),
        }
    }
}

#[repr(transparent)]
#[derive(Clone)]
pub struct MaybeBackendMessage(Option<Box<BackendMessage>>);

impl From<BackendMessage> for MaybeBackendMessage {
    fn from(msg: BackendMessage) -> Self {
        Self(Some(Box::new(msg.clone())))
    }
}

#[cfg(feature = "browser")]
pub mod browser_loader {
    // use js_sys::WebAssembly::instantiate_buffer;
}

#[cfg(not(feature = "browser"))]
pub mod default_loader {
    use std::fs;
    use std::sync::Arc;
    use std::sync::Mutex;

    use lazy_static::lazy_static;
    use wasmer::imports;
    use wasmer::ExternRef;
    use wasmer::FromToNativeWasmType;
    use wasmer::TypedFunction;

    use super::MaybeBackendMessage;
    use crate::backend::types::BackendMessage;
    use crate::error::Error;
    use crate::error::Result;

    lazy_static! {
        static ref WASM_MEM: Arc<Mutex<wasmer::Store>> =
            Arc::new(Mutex::new(wasmer::Store::default()));
    }

    unsafe impl FromToNativeWasmType for MaybeBackendMessage {
        type Native = Option<ExternRef>;

        fn from_native(native: Self::Native) -> Self {
            if native.is_none() {
                return Self(None);
            }
            match WASM_MEM
                .lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)
            {
                Ok(mem) => {
                    if let Some(m) = native.unwrap().downcast::<Self>(&mem) {
                        m.clone()
                    } else {
                        Self(None)
                    }
                }
                _ => Self(None),
            }
        }

        fn to_native(self) -> Self::Native {
            // Convert BackendMessage to the native representation
            if self.0.is_none() {
                return None;
            }

            match WASM_MEM
                .lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)
            {
                Ok(mut mem) => Some(ExternRef::new::<Self>(&mut mem, self)),
                _ => None,
            }
        }
    }

    pub struct Handler {
        pub func: TypedFunction<MaybeBackendMessage, MaybeBackendMessage>,
    }

    impl Handler {
        pub fn call(&self, msg: BackendMessage) -> Result<BackendMessage> {
            let msg: MaybeBackendMessage = msg.into();
            let mut mem = WASM_MEM
                .lock()
                .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
            let ret = self
                .func
                .call(&mut mem, msg)
                .map_err(|_| Error::WasmRuntimeError)?;
            if ret.0.is_none() {
                Err(Error::WasmRuntimeError)
            } else {
                Ok(*ret.0.unwrap())
            }
        }
    }

    pub fn load(wat: String) -> Result<Handler> {
        let mut store = WASM_MEM
            .lock()
            .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
        let module = wasmer::Module::new(&store, &wat).unwrap();
        // The module doesn't import anything, so we create an empty import object.
        let import_object = imports! {};
        let ins = wasmer::Instance::new(&mut store, &module, &import_object).unwrap();
        let handler: TypedFunction<MaybeBackendMessage, MaybeBackendMessage> = ins
            .exports
            .get_function("handler")
            .map_err(|_| Error::WasmExportError)?
            .typed(&mut store)
            .map_err(|_| Error::WasmRuntimeError)?;

        Ok(Handler { func: handler })
    }

    pub fn load_from_fs(path: String) -> Result<Handler> {
        if let Ok(wat) = fs::read_to_string(path) {
            load(wat)
        } else {
            Err(Error::WasmFailedToLoadFile)
        }
    }
}

#[cfg(test)]
mod test {
    use crate::backend::extension::loader::load_from_fs;
    use crate::backend::types::BackendMessage;
    use crate::backend::types::MessageType;

    #[test]
    fn test_load_wasm() {
	let path = "../target/wasm32-unknown-unknown/release/hello_extension.wat".to_string();
        let handler =
            load_from_fs(path)
                .unwrap();
        let data = "hello extension";
        let msg = BackendMessage::from((2u16, data.as_bytes()));
        let ret = handler.call(msg.clone()).unwrap();
        assert_eq!(ret, msg);
    }
}
