//! special supported structure and functions for browser and wasm
//! ===========
use std::str::FromStr;

use rings_snark::prelude::ff;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsError;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

use super::*;
use crate::backend::types::snark::SNARKProofTask;
use crate::backend::types::snark::SNARKVerifyTask;
use crate::backend::BackendMessageHandlerDynObj;
use crate::prelude::rings_core::utils::js_value;

/// We need this ref to pass Task ref to js_sys
#[wasm_bindgen]
#[derive(Deserialize, Serialize)]
pub struct SNARKProofTaskRef {
    inner: Arc<SNARKProofTask>,
}

#[wasm_bindgen]
impl SNARKProofTaskRef {
    /// Make snark proof task ref splitable
    pub fn split(&self, n: usize) -> Vec<SNARKProofTaskRef> {
        self.inner.split(n).into_iter().map(|t| t.into()).collect()
    }

    /// serialize SNARKProofTaskRef to json
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// deserialize SNARKProofTaskRef from json
    pub fn from_json(s: String) -> Result<SNARKProofTaskRef> {
        Ok(serde_json::from_str(&s)?)
    }
}

impl AsRef<SNARKProofTask> for SNARKProofTaskRef {
    fn as_ref(&self) -> &SNARKProofTask {
        self.inner.as_ref()
    }
}

impl std::ops::Deref for SNARKProofTaskRef {
    type Target = Arc<SNARKProofTask>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<SNARKProofTask> for SNARKProofTaskRef {
    fn from(t: SNARKProofTask) -> SNARKProofTaskRef {
        SNARKProofTaskRef { inner: Arc::new(t) }
    }
}

/// We need this ref to pass Task ref to js_sys
#[wasm_bindgen]
#[derive(Deserialize, Serialize)]
pub struct SNARKVerifyTaskRef {
    inner: Arc<SNARKVerifyTask>,
}

#[wasm_bindgen]
impl SNARKVerifyTaskRef {
    /// serialize SNARKPVerifyRef to json
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }

    /// de serialize SNARKPVerifyRef from json
    pub fn from_json(s: String) -> Result<SNARKVerifyTaskRef> {
        Ok(serde_json::from_str(&s)?)
    }
}

impl AsRef<SNARKVerifyTask> for SNARKVerifyTaskRef {
    fn as_ref(&self) -> &SNARKVerifyTask {
        self.inner.as_ref()
    }
}

impl std::ops::Deref for SNARKVerifyTaskRef {
    type Target = Arc<SNARKVerifyTask>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<SNARKVerifyTask> for SNARKVerifyTaskRef {
    fn from(t: SNARKVerifyTask) -> SNARKVerifyTaskRef {
        SNARKVerifyTaskRef { inner: Arc::new(t) }
    }
}

#[wasm_bindgen]
impl SNARKVerifyTaskRef {
    /// Clone snark verify ref, and hold the arc
    /// this function is useful on js_sys
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> SNARKVerifyTaskRef {
        SNARKVerifyTaskRef {
            inner: self.inner.clone(),
        }
    }
}

#[wasm_bindgen]
impl SNARKProofTaskRef {
    /// Clone snark proof ref, and hold the arc
    /// this function is useful on js_sys
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> SNARKProofTaskRef {
        SNARKProofTaskRef {
            inner: self.inner.clone(),
        }
    }
}

#[wasm_bindgen]
impl SNARKBehaviour {
    /// Get behaviour as dyn obj ref
    pub fn as_dyn_obj(self) -> BackendMessageHandlerDynObj {
        BackendMessageHandlerDynObj::new(self.into())
    }

    /// Handle js native message
    pub fn handle_snark_task_message(
        self,
        provider: Provider,
        ctx: JsValue,
        msg: JsValue,
    ) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let ctx = js_value::deserialize::<MessagePayload>(ctx)?;
            let msg = js_value::deserialize::<SNARKTaskMessage>(msg)?;
            ins.handle_message(provider.into(), &ctx, &msg)
                .await
                .map_err(|e| Error::BackendError(e.to_string()))?;
            Ok(JsValue::NULL)
        })
    }

    /// gen proof task with circuits, this function is use for solo proof
    /// you can call [SNARKBehaviour::handle_snark_proof_task_ref] later to finalize the proof
    pub fn gen_proof_task_ref(circuits: Vec<Circuit>) -> Result<SNARKProofTaskRef> {
        SNARKBehaviour::gen_proof_task(circuits).map(|t| t.into())
    }

    /// handle snark proof task ref, this function is helpful for js_sys
    pub fn handle_snark_proof_task_ref(data: SNARKProofTaskRef) -> Result<SNARKVerifyTaskRef> {
        Self::handle_snark_proof_task(data).map(|x| x.into())
    }

    /// handle snark verify task ref, this function is helpful for js_sys
    pub fn handle_snark_verify_task_ref(
        data: SNARKVerifyTaskRef,
        snark: SNARKProofTaskRef,
    ) -> Result<bool> {
        Self::handle_snark_verify_task(data, snark)
    }

    /// send proof task to did
    pub fn send_proof_task_to(
        &self,
        provider: Provider,
        task: SNARKProofTaskRef,
        did: String,
    ) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let ret = ins
                .send_proof_task(provider.clone().into(), task.as_ref(), Did::from_str(&did)?)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from(ret))
        })
    }

    /// Generate a proof task and send it to did
    pub fn gen_and_send_proof_task_to(
        &self,
        provider: Provider,
        circuits: Vec<Circuit>,
        did: String,
    ) -> js_sys::Promise {
        let ins = self.clone();
        future_to_promise(async move {
            let ret = ins
                .gen_and_send_proof_task(provider.clone().into(), circuits, Did::from_str(&did)?)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from(ret))
        })
    }

    /// create new instance for browser
    /// which support syntax `new SNARKBehaviour` in browser env
    #[wasm_bindgen(constructor)]
    pub fn new_instance() -> SNARKBehaviour {
        SNARKBehaviour::default()
    }

    /// Clone snarkbehaviour, and hold the arc
    /// this function is useful on js_sys
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> SNARKBehaviour {
        SNARKBehaviour {
            inner: self.inner.clone(),
        }
    }
}

#[wasm_bindgen]
impl SNARKTaskBuilder {
    /// create new instance for browser
    /// which support syntax `new SNARKTaskBuilder` in browser env
    #[wasm_bindgen(constructor)]
    pub fn new_instance(
        r1cs_path: String,
        witness_wasm_path: String,
        field: SupportedPrimeField,
    ) -> js_sys::Promise {
        future_to_promise(async move {
            let ret = SNARKTaskBuilder::from_remote(r1cs_path, witness_wasm_path, field)
                .await
                .map_err(JsError::from)?;
            Ok(JsValue::from(ret))
        })
    }
}

/// Convert biguint to finatefield
pub(crate) fn bigint2ff<F: ff::PrimeField>(v: js_sys::BigInt) -> Result<F> {
    let repr = v
        .to_string(10)
        .map_err(|e| Error::SNARKFFRangeError(format!("{:?}", e)))?
        .as_string();
    if let Some(v) = &repr {
        Ok(F::from_str_vartime(v).ok_or(Error::FailedToLoadFF())?)
    } else {
        Err(Error::SNARKBigIntValueEmpty())
    }
}

/// Convert BigInt from js to [Field]
#[wasm_bindgen]
pub fn bigint_to_field(v: js_sys::BigInt, field: SupportedPrimeField) -> Result<Field> {
    let ret = match field {
        SupportedPrimeField::Vesta => {
            type F = <provider::VestaEngine as Engine>::Scalar;
            Field {
                value: FieldEnum::Vesta(bigint2ff::<F>(v)?),
            }
        }
        SupportedPrimeField::Pallas => {
            type F = <provider::PallasEngine as Engine>::Scalar;
            Field {
                value: FieldEnum::Pallas(bigint2ff::<F>(v)?),
            }
        }
        SupportedPrimeField::Bn256KZG => {
            type F = <provider::hyperkzg::Bn256EngineKZG as Engine>::Scalar;
            Field {
                value: FieldEnum::Bn256KZG(bigint2ff::<F>(v)?),
            }
        }
    };
    Ok(ret)
}

#[wasm_bindgen]
impl Input {
    /// Convert [["foo", [BigInt(2), BigInt(3)]], ["bar", [BigInt(4), BigInt(5)]]] to Input with given field
    pub fn from_array(input: js_sys::Array, field: SupportedPrimeField) -> Input {
        let data: Vec<(String, Vec<Field>)> = input
            .into_iter()
            .map(|s| {
                let lst = js_sys::Array::from(&s);
                let p = lst
                    .get(0)
                    .as_string()
                    .expect("first argument should be string like");
                let v: Vec<Field> = js_sys::Array::from(&lst.get(1))
                    .into_iter()
                    .map(|p| {
                        bigint_to_field(p.into(), field.clone())
                            .expect("failed to cover bigint to field")
                    })
                    .collect();
                (p, v)
            })
            .collect();
        Input(data)
    }
}
