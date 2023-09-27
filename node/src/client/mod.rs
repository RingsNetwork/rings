use std::ffi::c_char;
use std::ffi::CStr;
use std::ffi::CString;

use std::sync::Arc;

use futures::executor;
use rings_core::async_trait;
use rings_core::message::CustomMessage;
use rings_core::message::Message;
use rings_core::message::MessageCallback;
use rings_core::message::MessageHandlerEvent;
use rings_core::message::MessagePayload;
use rings_core::session::SessionSkBuilder;
use rings_core::storage::PersistenceStorage;


use crate::error::Error;
use crate::error::Result;
use crate::jsonrpc::handler::MethodHandler;
use crate::jsonrpc::HandlerType;
use crate::measure::PeriodicMeasure;

use crate::prelude::jsonrpc_core::types::id::Id;

use crate::prelude::jsonrpc_core::MethodCall;
use crate::prelude::wasm_export;

use crate::processor::Processor;
use crate::processor::ProcessorBuilder;
use crate::processor::ProcessorConfig;

/// Boxed Callback, for non-wasm, it should be Sized, Send and Sync.
#[cfg(not(feature = "browser"))]
pub type TyMessageCallback = Box<dyn MessageCallback + Send + Sync + 'static>;

/// Boxed Callback
#[cfg(feature = "browser")]
pub type TyMessageCallback = Box<dyn MessageCallback + 'static>;

/// AddressType enum contains `DEFAULT` and `ED25519`.
#[repr(C)]
#[wasm_export]
pub enum AddressTypeFFI {
    DEFAULT,
    Ed25519,
}

#[derive(Clone)]
#[allow(dead_code)]
#[repr(C)]
#[wasm_export]
pub struct ClientFFI {
    processor: Arc<Processor>,
    handler: Arc<HandlerType>,
}

#[repr(C)]
pub struct ClientFFIPtr {
    processor: *const Processor,
    handler: *const HandlerType,
}

impl ClientFFI {
    pub unsafe fn from_ptr(ptr: &ClientFFIPtr) -> ClientFFI {
        let processor = unsafe { Arc::<Processor>::from_raw(ptr.processor as *const Processor) };
        let handler = unsafe { Arc::<HandlerType>::from_raw(ptr.handler as *const HandlerType) };
        Self { processor, handler }
    }

    pub fn into_ptr(&self) -> ClientFFIPtr {
        ClientFFIPtr {
            processor: Arc::as_ptr(&self.processor),
            handler: Arc::as_ptr(&self.handler),
        }
    }
}

#[cfg(not(feature = "browser"))]
unsafe impl Send for MessageCallbackInstanceFFI {}
#[cfg(not(feature = "browser"))]
unsafe impl Sync for MessageCallbackInstanceFFI {}

#[repr(C)]
pub struct MessageCallbackInstanceFFI {
    custom_message_callback: Option<extern "C" fn(*const c_char, *const c_char) -> ()>,
    builtin_message_callback: Option<extern "C" fn(*const c_char) -> ()>,
}

#[no_mangle]
pub extern "C" fn new_callback(
    custom_message_cb: Option<extern "C" fn(*const c_char, *const c_char) -> ()>,
    builtin_message_cb: Option<extern "C" fn(*const c_char) -> ()>,
) -> MessageCallbackInstanceFFI {
    MessageCallbackInstanceFFI {
        custom_message_callback: custom_message_cb,
        builtin_message_callback: builtin_message_cb,
    }
}

#[no_mangle]
pub unsafe extern "C" fn request(
    client: &ClientFFIPtr,
    method: *const c_char,
    request: *const c_char,
) -> *const c_char {
    match (|| -> Result<*const c_char> {
	let client: ClientFFI = unsafe {ClientFFI::from_ptr(client) };
	let c_method = unsafe { CStr::from_ptr(method) };
	let c_request = unsafe { CStr::from_ptr(request) };
	let method = c_method.to_str()?.to_owned();
	let request = c_request.to_str()?.to_owned();
	let ret: String = executor::block_on(client.request_internal(method, request, None))?;
	let c_ret = CString::new(ret)?;
	Ok(c_ret.as_ptr())
    })() {
	Ok(r) => r,
        Err(e) => {
            panic!("FFI: Failed on request {:#}", e)
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn new_client_with_callback(
    ice_server: *const c_char,
    stabilize_timeout: usize,
    account: *const c_char,
    account_type: *const c_char,
    signer: extern "C" fn(*const c_char) -> *const c_char,
    callback: MessageCallbackInstanceFFI,
) -> ClientFFIPtr {
    fn wrapped_signer(
        signer: extern "C" fn(*const c_char) -> *const c_char,
    ) -> impl Fn(String) -> Vec<u8> {

        move |data: String| -> Vec<u8> {
            let c_data = CString::new(data).expect("Failed on covering String to CString");
            let sig = signer(c_data.as_ptr());
            let c_ret = unsafe { CStr::from_ptr(sig) };
            let ret = c_ret.to_str().expect("Failed on covering CStr to Str");
            ret.into()
        }
    }

    let ret = match (|| -> Result<ClientFFI> {
        let c_ice = unsafe { CStr::from_ptr(ice_server) };
        let c_acc = unsafe { CStr::from_ptr(account) };
        let c_acc_ty = unsafe { CStr::from_ptr(account_type) };

        let ice: String = c_ice.to_str()?.to_owned();
        let acc: String = c_acc.to_str()?.to_owned();
        let acc_ty: String = c_acc_ty.to_str()?.to_owned();

        executor::block_on(ClientFFI::new_client_internal(
            ice,
            stabilize_timeout,
            acc,
            acc_ty,
            Box::new(wrapped_signer(signer)),
            Some(callback),
        ))
    })() {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed on create new client {:#}", e)
        }
    };
    ret.into_ptr()
}

#[cfg_attr(feature = "browser", async_trait(?Send))]
#[cfg_attr(not(feature = "browser"), async_trait)]
impl MessageCallback for MessageCallbackInstanceFFI {
    async fn custom_message(
        &self,
        relay: &MessagePayload<Message>,
        msg: &CustomMessage,
    ) -> Vec<MessageHandlerEvent> {
        match (|| -> Result<()> {
            let relay = serde_json::to_string(relay)?;
            let msg = serde_json::to_string(msg)?;
            if let Some(cb) = self.custom_message_callback {
                let _relay = CString::new(relay)?;
                let _msg = CString::new(msg)?;
                cb(_relay.as_ptr(), _msg.as_ptr())
            };
            Ok(())
        })() {
            Ok(()) => (),
            Err(e) => {
                log::error!("Failed on handle builtin_message {:#}", e);
            }
        }
        vec![]
    }

    async fn builtin_message(&self, relay: &MessagePayload<Message>) -> Vec<MessageHandlerEvent> {
        match (|| -> Result<()> {
            let relay = serde_json::to_string(relay)?;
            if let Some(cb) = self.builtin_message_callback {
                let _relay = CString::new(relay)?;
                cb(_relay.as_ptr())
            };
            Ok(())
        })() {
            Ok(()) => (),
            Err(e) => {
                log::error!("Failed on handle builtin_message {:#}", e);
            }
        };
        vec![]
    }
}

#[allow(dead_code)]
impl ClientFFI {
    pub(crate) fn new_client_with_storage_internal(
        config: ProcessorConfig,
        cb: Option<TyMessageCallback>,
        storage_name: String,
    ) -> Result<ClientFFI> {
        let storage_path = storage_name.as_str();
        let measure_path = [storage_path, "measure"].join("/");

        let storage = executor::block_on(PersistenceStorage::new_with_cap_and_name(
            50000,
            storage_path,
        ))
        .map_err(Error::Storage)?;

        let ms = executor::block_on(PersistenceStorage::new_with_cap_and_path(
            50000,
            measure_path,
        ))
        .map_err(Error::Storage)?;
        let measure = PeriodicMeasure::new(ms);

        let mut processor_builder = ProcessorBuilder::from_config(&config)?
            .storage(storage)
            .measure(measure);

        if let Some(cb) = cb {
            processor_builder = processor_builder.message_callback(cb);
        }

        let processor = Arc::new(processor_builder.build()?);

        let mut handler: HandlerType = processor.clone().into();
        handler.build();

        Ok(ClientFFI {
            processor,
            handler: handler.into(),
        })
    }

    pub(crate) fn new_client_with_storage_and_serialized_config_internal(
        config: String,
        callback: Option<TyMessageCallback>,
        storage_name: String,
    ) -> Result<ClientFFI> {
        let config: ProcessorConfig = serde_yaml::from_str(&config)?;
        Self::new_client_with_storage_internal(config, callback, storage_name)
    }

    pub(crate) async fn new_client_internal(
        ice_servers: String,
        stabilize_timeout: usize,
        account: String,
        account_type: String,
        signer: Box<dyn Fn(String) -> Vec<u8>>,
        callback: Option<MessageCallbackInstanceFFI>,
    ) -> Result<ClientFFI> {
        let mut sk_builder = SessionSkBuilder::new(account, account_type);
        let proof = sk_builder.unsigned_proof();
        let sig = signer(proof);
        sk_builder = sk_builder.set_session_sig(sig.to_vec());
        let session_sk = sk_builder.build().unwrap();
        let config = ProcessorConfig::new(ice_servers, session_sk, stabilize_timeout);
        let cb: Option<TyMessageCallback> = if let Some(cb) = callback {
            let cb: TyMessageCallback = Box::new(cb);
            Some(cb)
        } else {
            None
        };
        Self::new_client_with_storage_internal(config, cb, "rings-node".to_string())
    }

    /// Request local rpc interface
    pub(crate) async fn request_internal(
        &self,
        method: String,
        params: String,
        opt_id: Option<String>,
    ) -> Result<String> {
        let handler = self.handler.clone();
        let params = serde_json::from_str(&params)?;
        let id = if let Some(id) = opt_id {
            Id::Str(id)
        } else {
            Id::Null
        };
        let req: MethodCall = MethodCall {
            jsonrpc: None,
            method,
            params,
            id,
        };
        serde_json::to_string(
            &handler
                .handle_request(req)
                .await
                .map_err(Error::InternalRpcError)?,
        )
        .map_err(Error::SerdeJsonError)
    }
}
