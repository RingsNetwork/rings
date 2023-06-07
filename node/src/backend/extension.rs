
use std::sync::Arc;
use std::sync::Mutex;

use lazy_static::lazy_static;
use wasmer::imports;
use wasmer::ExternRef;
use wasmer::FromToNativeWasmType;
use wasmer::TypedFunction;

use crate::backend::types::BackendMessage;
use crate::error::Error;
use crate::error::Result;

lazy_static! {
    static ref WASM_MEM: Arc<Mutex<wasmer::Store>> = Arc::new(Mutex::new(wasmer::Store::default()));
}

#[derive(Debug, Clone)]
pub struct MaybeBackendMessage(Option<BackendMessage>);

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

pub fn loader(wat: String) -> Result<Handler> {
    let mut store = WASM_MEM
        .lock()
        .map_err(|_| Error::WasmGlobalMemoryMutexError)?;
    let module = wasmer::Module::new(&store, &wat).map_err(|_| Error::WasmCompileError)?;
    // The module doesn't import anything, so we create an empty import object.
    let import_object = imports! {};
    let ins = wasmer::Instance::new(&mut store, &module, &import_object)
        .map_err(|_| Error::WasmInstantiationError)?;
    let handler: TypedFunction<MaybeBackendMessage, MaybeBackendMessage> = ins
        .exports
        .get_function("handler")
        .map_err(|_| Error::WasmExportError)?
        .typed(&mut store)
        .map_err(|_| Error::WasmRuntimeError)?;

    Ok(Handler { func: handler })
}
