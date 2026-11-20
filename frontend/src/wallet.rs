//! Browser account standards used to authorize a Rings delegatee key.

use std::cell::RefCell;
use std::rc::Rc;

use base58::FromBase58;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use futures::channel::oneshot;
use js_sys::Array;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use rings_node::prelude::rings_core::ecc::signers::secp256r1;
use rings_node::prelude::rings_core::ecc::AccountVerifier;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::Event;

use crate::browser_api::await_js;
use crate::browser_api::is_callable;
use crate::browser_api::js_call0;
use crate::browser_api::js_call1;
use crate::browser_api::js_call2;
use crate::browser_api::js_call3;
use crate::browser_api::js_error_label;
use crate::browser_api::js_global_prop;
use crate::browser_api::js_prop;
use crate::browser_api::js_set;
use crate::browser_api::js_string_field;
use crate::hex;

const EXTENSION_WALLET_BRIDGE: &str = "RingsExtensionWalletBridge";
const WEBCRYPTO_IDB_DATABASE: &str = "rings-frontend-webcrypto";
const WEBCRYPTO_IDB_STORE: &str = "keys";
const WEBCRYPTO_IDB_PRIMARY_KEY: &str = "p256-default";
const WEBCRYPTO_IDB_VERSION: u32 = 1;

/// Account authorization standard selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletKind {
    /// Browser-native P-256 key generated with WebCrypto.
    WebCrypto,
    /// EIP-191 signature through an EIP-1193 Ethereum provider.
    EthereumEip191,
    /// Ed25519 signature through a Solana provider.
    SolanaEd25519,
}

/// Connected browser account and the opaque signing handle.
///
/// `Clone` duplicates a JavaScript object reference, not private key material.
/// Each clone carries the same signing authority, so clone only across UI state
/// handles that need to observe or invoke the same connected account.
#[derive(Clone)]
pub struct WalletAccount {
    /// Account standard that created this account.
    pub kind: WalletKind,
    /// Account entity passed to `DelegationBuilder`.
    pub account: String,
    /// Lower-case Rings account type.
    pub account_type: String,
    handle: JsValue,
}

impl WalletKind {
    /// Parse a UI value.
    pub fn from_value(value: &str) -> Self {
        match value {
            "eip191" | "metamask" => Self::EthereumEip191,
            "ed25519" | "phantom" => Self::SolanaEd25519,
            _ => Self::WebCrypto,
        }
    }

    /// UI value.
    pub fn value(self) -> &'static str {
        match self {
            Self::WebCrypto => "webcrypto",
            Self::EthereumEip191 => "eip191",
            Self::SolanaEd25519 => "ed25519",
        }
    }

    /// Human label.
    pub fn label(self) -> &'static str {
        match self {
            Self::WebCrypto => "WebCrypto P-256",
            Self::EthereumEip191 => "Ethereum EIP-191",
            Self::SolanaEd25519 => "Solana Ed25519",
        }
    }
}

impl WalletAccount {
    /// Build a display-only account view for an already running extension node.
    pub fn extension_view(
        kind: WalletKind,
        account: String,
        account_type: String,
        handle: JsValue,
    ) -> Self {
        Self {
            kind,
            account,
            account_type,
            handle,
        }
    }

    /// Sign the delegation proof expected by `DelegationBuilder`.
    pub async fn sign_session_proof(&self, proof: &str) -> Result<Vec<u8>, String> {
        match self.kind {
            WalletKind::WebCrypto => sign_webcrypto(&self.handle, proof).await,
            WalletKind::EthereumEip191 => sign_eip191(&self.handle, &self.account, proof).await,
            WalletKind::SolanaEd25519 => sign_ed25519(&self.handle, proof).await,
        }
    }
}

/// Connect the selected provider and return its Rings account metadata.
pub async fn connect(kind: WalletKind) -> Result<WalletAccount, String> {
    match kind {
        WalletKind::WebCrypto => connect_webcrypto().await,
        WalletKind::EthereumEip191 => connect_eip191().await,
        WalletKind::SolanaEd25519 => connect_ed25519().await,
    }
}

fn request(provider: &JsValue, method_name: &str, params: JsValue) -> Result<JsValue, String> {
    let body = Object::new();
    js_set(&body, "method", &JsValue::from_str(method_name))?;
    js_set(&body, "params", &params)?;
    js_call1(provider, "request", &body.into())
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    let clean = hex.strip_prefix("0x").unwrap_or(hex);
    if !clean.len().is_multiple_of(2) || !clean.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("wallet returned an invalid hex signature".to_string());
    }
    clean
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let [high, low] = pair else {
                return Err(());
            };
            let high = hex::hex_nibble(*high).ok_or(())?;
            let low = hex::hex_nibble(*low).ok_or(())?;
            Ok((high << 4) | low)
        })
        .collect::<Result<Vec<_>, ()>>()
        .map_err(|()| "wallet returned an invalid hex signature".to_string())
}

fn base64_url_to_bytes(value: &str) -> Result<Vec<u8>, String> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|error| format!("invalid base64url field: {error}"))
}

fn rings_prefixed_message(message: &str) -> Vec<u8> {
    let body = message.as_bytes();
    let mut out = format!("\x19Rings Signed Message:\n{}", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}

fn ecdsa_algorithm() -> Result<Object, String> {
    let algorithm = Object::new();
    js_set(&algorithm, "name", &JsValue::from_str("ECDSA"))?;
    js_set(&algorithm, "namedCurve", &JsValue::from_str("P-256"))?;
    Ok(algorithm)
}

async fn connect_webcrypto() -> Result<WalletAccount, String> {
    let crypto = js_global_prop("crypto")?;
    let subtle = js_prop(&crypto, "subtle")?;
    if subtle.is_undefined() || subtle.is_null() {
        return Err("WebCrypto SubtleCrypto is not available".to_string());
    }

    if let Ok(Some(stored)) = load_persisted_webcrypto_account().await {
        return Ok(stored);
    }

    let account = generate_webcrypto_account(&subtle).await?;
    let _ = persist_webcrypto_account(&account).await;
    Ok(account)
}

async fn generate_webcrypto_account(subtle: &JsValue) -> Result<WalletAccount, String> {
    let usages = Array::new();
    usages.push(&JsValue::from_str("sign"));
    let key_pair = await_js(js_call3(
        subtle,
        "generateKey",
        &ecdsa_algorithm()?.into(),
        &JsValue::FALSE,
        &usages.into(),
    )?)
    .await?;
    let public_key = js_prop(&key_pair, "publicKey")?;
    let private_key = js_prop(&key_pair, "privateKey")?;
    let jwk = await_js(js_call2(
        subtle,
        "exportKey",
        &JsValue::from_str("jwk"),
        &public_key,
    )?)
    .await?;
    let x = base64_url_to_bytes(&js_string_field(&jwk, "x")?)?;
    let y = base64_url_to_bytes(&js_string_field(&jwk, "y")?)?;
    let mut public = x;
    public.extend_from_slice(&y);

    Ok(WalletAccount {
        kind: WalletKind::WebCrypto,
        account: bytes_to_hex(&public),
        account_type: "secp256r1".to_string(),
        handle: private_key,
    })
}

struct IdbRequestWatch {
    request: JsValue,
    _handlers: [Closure<dyn FnMut(Event)>; 2],
    receiver: oneshot::Receiver<Result<JsValue, String>>,
}

impl IdbRequestWatch {
    fn new(request: &JsValue) -> Self {
        let (sender, receiver) = oneshot::channel::<Result<JsValue, String>>();
        let outcome = Rc::new(RefCell::new(Some(sender)));
        let on_success = {
            let outcome = outcome.clone();
            let request = request.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                if let Some(tx) = outcome.borrow_mut().take() {
                    let _ = tx.send(js_prop(&request, "result"));
                }
            })
        };
        let on_error = {
            let outcome = outcome.clone();
            let request = request.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                if let Some(tx) = outcome.borrow_mut().take() {
                    let error = js_prop(&request, "error")
                        .map(js_error_label)
                        .unwrap_or_else(|err| err);
                    let _ = tx.send(Err(error));
                }
            })
        };
        let _ = Reflect::set(
            request,
            &JsValue::from_str("onsuccess"),
            on_success.as_ref().unchecked_ref(),
        );
        let _ = Reflect::set(
            request,
            &JsValue::from_str("onerror"),
            on_error.as_ref().unchecked_ref(),
        );
        Self {
            request: request.clone(),
            _handlers: [on_success, on_error],
            receiver,
        }
    }
}

impl Drop for IdbRequestWatch {
    fn drop(&mut self) {
        let _ = Reflect::set(
            &self.request,
            &JsValue::from_str("onsuccess"),
            &JsValue::NULL,
        );
        let _ = Reflect::set(&self.request, &JsValue::from_str("onerror"), &JsValue::NULL);
    }
}

struct IdbUpgradeWatch {
    open: JsValue,
    _handler: Closure<dyn FnMut(Event)>,
}

impl Drop for IdbUpgradeWatch {
    fn drop(&mut self) {
        let _ = Reflect::set(
            &self.open,
            &JsValue::from_str("onupgradeneeded"),
            &JsValue::NULL,
        );
    }
}

struct IdbDatabaseGuard {
    db: JsValue,
}

impl Drop for IdbDatabaseGuard {
    fn drop(&mut self) {
        let _ = js_call0(&self.db, "close");
    }
}

struct IdbTransactionWatch {
    transaction: JsValue,
    _handlers: [Closure<dyn FnMut(Event)>; 3],
    receiver: oneshot::Receiver<Result<(), String>>,
    settled: bool,
}

impl IdbTransactionWatch {
    fn new(transaction: &JsValue) -> Self {
        let (sender, receiver) = oneshot::channel::<Result<(), String>>();
        let outcome = Rc::new(RefCell::new(Some(sender)));
        let on_complete = {
            let outcome = outcome.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                if let Some(tx) = outcome.borrow_mut().take() {
                    let _ = tx.send(Ok(()));
                }
            })
        };
        let on_error = {
            let outcome = outcome.clone();
            let transaction = transaction.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                if let Some(tx) = outcome.borrow_mut().take() {
                    let error = js_prop(&transaction, "error")
                        .map(js_error_label)
                        .unwrap_or_else(|err| err);
                    let _ = tx.send(Err(error));
                }
            })
        };
        let on_abort = {
            let outcome = outcome.clone();
            Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
                if let Some(tx) = outcome.borrow_mut().take() {
                    let _ = tx.send(Err("IndexedDB transaction aborted".to_string()));
                }
            })
        };
        let _ = Reflect::set(
            transaction,
            &JsValue::from_str("oncomplete"),
            on_complete.as_ref().unchecked_ref(),
        );
        let _ = Reflect::set(
            transaction,
            &JsValue::from_str("onerror"),
            on_error.as_ref().unchecked_ref(),
        );
        let _ = Reflect::set(
            transaction,
            &JsValue::from_str("onabort"),
            on_abort.as_ref().unchecked_ref(),
        );
        Self {
            transaction: transaction.clone(),
            _handlers: [on_complete, on_error, on_abort],
            receiver,
            settled: false,
        }
    }

    async fn committed(mut self) -> Result<(), String> {
        let result = (&mut self.receiver)
            .await
            .map_err(|_| "IndexedDB transaction dropped before settling".to_string())?;
        self.settled = true;
        result
    }
}

impl Drop for IdbTransactionWatch {
    fn drop(&mut self) {
        let _ = Reflect::set(
            &self.transaction,
            &JsValue::from_str("oncomplete"),
            &JsValue::NULL,
        );
        let _ = Reflect::set(
            &self.transaction,
            &JsValue::from_str("onerror"),
            &JsValue::NULL,
        );
        let _ = Reflect::set(
            &self.transaction,
            &JsValue::from_str("onabort"),
            &JsValue::NULL,
        );
        if !self.settled {
            let _ = js_call0(&self.transaction, "abort");
        }
    }
}

async fn open_webcrypto_idb() -> Result<JsValue, String> {
    let factory = js_global_prop("indexedDB")?;
    if factory.is_undefined() || factory.is_null() {
        return Err("IndexedDB is unavailable".to_string());
    }
    let open = js_call2(
        &factory,
        "open",
        &JsValue::from_str(WEBCRYPTO_IDB_DATABASE),
        &JsValue::from_f64(f64::from(WEBCRYPTO_IDB_VERSION)),
    )?;
    let on_upgrade = {
        let open = open.clone();
        Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            let Ok(db) = js_prop(&open, "result") else {
                return;
            };
            let has_store = js_prop(&db, "objectStoreNames")
                .and_then(|names| {
                    js_call1(&names, "contains", &JsValue::from_str(WEBCRYPTO_IDB_STORE))
                })
                .ok()
                .and_then(|val| val.as_bool())
                .unwrap_or(false);
            if !has_store {
                let _ = js_call1(
                    &db,
                    "createObjectStore",
                    &JsValue::from_str(WEBCRYPTO_IDB_STORE),
                );
            }
        })
    };
    let _ = Reflect::set(
        &open,
        &JsValue::from_str("onupgradeneeded"),
        on_upgrade.as_ref().unchecked_ref(),
    );
    let _upgrade_watch = IdbUpgradeWatch {
        open: open.clone(),
        _handler: on_upgrade,
    };
    await_idb_request(&open).await
}

async fn await_idb_request(request: &JsValue) -> Result<JsValue, String> {
    let mut watch = IdbRequestWatch::new(request);
    (&mut watch.receiver)
        .await
        .map_err(|_| "IndexedDB request dropped before settling".to_string())?
}

async fn load_persisted_webcrypto_account() -> Result<Option<WalletAccount>, String> {
    let db = IdbDatabaseGuard {
        db: open_webcrypto_idb().await?,
    };
    let transaction = js_call2(
        &db.db,
        "transaction",
        &JsValue::from_str(WEBCRYPTO_IDB_STORE),
        &JsValue::from_str("readonly"),
    )?;
    let store = js_call1(
        &transaction,
        "objectStore",
        &JsValue::from_str(WEBCRYPTO_IDB_STORE),
    )?;
    let get_req = js_call1(&store, "get", &JsValue::from_str(WEBCRYPTO_IDB_PRIMARY_KEY))?;
    let record = await_idb_request(&get_req).await?;
    if record.is_undefined() || record.is_null() {
        return Ok(None);
    }
    let account = js_string_field(&record, "account")?;
    let private_key = js_prop(&record, "privateKey")?;
    if account.trim().is_empty() || private_key.is_undefined() || private_key.is_null() {
        return Ok(None);
    }
    let candidate = WalletAccount {
        kind: WalletKind::WebCrypto,
        account,
        account_type: "secp256r1".to_string(),
        handle: private_key,
    };
    let verifier = AccountVerifier::from_account_parts(&candidate.account, &candidate.account_type)
        .map_err(|error| format!("persisted WebCrypto public key invalid: {error}"))?;
    let probe = "rings-webcrypto-key-check";
    let sig = candidate.sign_session_proof(probe).await?;
    if !verifier.verify(probe.as_bytes(), &sig) {
        return Err("persisted WebCrypto private key does not match public key".to_string());
    }
    Ok(Some(candidate))
}

async fn persist_webcrypto_account(account: &WalletAccount) -> Result<(), String> {
    let db = IdbDatabaseGuard {
        db: open_webcrypto_idb().await?,
    };
    let transaction = js_call2(
        &db.db,
        "transaction",
        &JsValue::from_str(WEBCRYPTO_IDB_STORE),
        &JsValue::from_str("readwrite"),
    )?;
    let commit = IdbTransactionWatch::new(&transaction);
    let store = js_call1(
        &transaction,
        "objectStore",
        &JsValue::from_str(WEBCRYPTO_IDB_STORE),
    )?;
    let record = Object::new();
    js_set(&record, "account", &JsValue::from_str(&account.account))?;
    js_set(&record, "privateKey", &account.handle)?;
    let put_req = js_call2(
        &store,
        "put",
        &record.into(),
        &JsValue::from_str(WEBCRYPTO_IDB_PRIMARY_KEY),
    )?;
    await_idb_request(&put_req).await?;
    commit.committed().await
}

async fn sign_webcrypto(private_key: &JsValue, proof: &str) -> Result<Vec<u8>, String> {
    let crypto = js_global_prop("crypto")?;
    let subtle = js_prop(&crypto, "subtle")?;
    let hash = Object::new();
    js_set(&hash, "name", &JsValue::from_str("SHA-256"))?;
    let algorithm = Object::new();
    js_set(&algorithm, "name", &JsValue::from_str("ECDSA"))?;
    js_set(&algorithm, "hash", &hash.into())?;
    let message = Uint8Array::from(rings_prefixed_message(proof).as_slice());
    let signature = await_js(js_call3(
        &subtle,
        "sign",
        &algorithm.into(),
        private_key,
        &message.into(),
    )?)
    .await?;
    normalize_webcrypto_signature(Uint8Array::new(&signature).to_vec())
}

fn normalize_webcrypto_signature(signature: Vec<u8>) -> Result<Vec<u8>, String> {
    secp256r1::normalize_signature(&signature)
        .map(|signature| signature.to_vec())
        .map_err(|error| format!("wallet returned an invalid secp256r1 signature: {error}"))
}

async fn connect_eip191() -> Result<WalletAccount, String> {
    if let Some(bridge) = extension_wallet_bridge() {
        return connect_extension_wallet(&bridge, WalletKind::EthereumEip191, "eip191", "eip191")
            .await;
    }

    let ethereum = js_global_prop("ethereum")?;
    if ethereum.is_undefined() || ethereum.is_null() {
        return Err("EIP-1193 Ethereum provider not found".to_string());
    }
    let accounts = await_js(request(
        &ethereum,
        "eth_requestAccounts",
        Array::new().into(),
    )?)
    .await?;
    let account = Array::from(&accounts)
        .get(0)
        .as_string()
        .ok_or_else(|| "Ethereum provider returned no account".to_string())?;
    Ok(WalletAccount {
        kind: WalletKind::EthereumEip191,
        account,
        account_type: "eip191".to_string(),
        handle: ethereum,
    })
}

async fn sign_eip191(ethereum: &JsValue, account: &str, proof: &str) -> Result<Vec<u8>, String> {
    if is_extension_wallet_bridge(ethereum) {
        let signed = sign_extension_wallet(ethereum, "eip191", proof, Some(account)).await?;
        let signature = js_prop(&signed, "signature")
            .unwrap_or(signed)
            .as_string()
            .ok_or_else(|| "EIP-191 bridge returned a non-string signature".to_string())?;
        return hex_to_bytes(&signature);
    }

    let params = Array::new();
    params.push(&JsValue::from_str(proof));
    params.push(&JsValue::from_str(account));
    let signature = await_js(request(ethereum, "personal_sign", params.into())?).await?;
    let signature = signature
        .as_string()
        .ok_or_else(|| "Ethereum provider returned a non-string signature".to_string())?;
    hex_to_bytes(&signature)
}

fn solana_provider() -> Result<JsValue, String> {
    let phantom = js_global_prop("phantom").unwrap_or(JsValue::UNDEFINED);
    let nested = if phantom.is_undefined() || phantom.is_null() {
        JsValue::UNDEFINED
    } else {
        js_prop(&phantom, "solana").unwrap_or(JsValue::UNDEFINED)
    };
    if !nested.is_undefined() && !nested.is_null() {
        return Ok(nested);
    }
    let solana = js_global_prop("solana")?;
    if solana.is_undefined() || solana.is_null() {
        Err("Solana provider not found".to_string())
    } else {
        Ok(solana)
    }
}

async fn connect_ed25519() -> Result<WalletAccount, String> {
    if let Some(bridge) = extension_wallet_bridge() {
        return connect_extension_wallet(&bridge, WalletKind::SolanaEd25519, "ed25519", "ed25519")
            .await;
    }

    let provider = solana_provider()?;
    await_js(js_call0(&provider, "connect")?).await?;
    let public_key = js_prop(&provider, "publicKey")?;
    let account = js_call0(&public_key, "toBase58")?
        .as_string()
        .ok_or_else(|| "Solana provider returned no public key".to_string())?;
    Ok(WalletAccount {
        kind: WalletKind::SolanaEd25519,
        account,
        account_type: "ed25519".to_string(),
        handle: provider,
    })
}

async fn sign_ed25519(provider: &JsValue, proof: &str) -> Result<Vec<u8>, String> {
    if is_extension_wallet_bridge(provider) {
        let signed = sign_extension_wallet(provider, "ed25519", proof, None).await?;
        let signature = js_prop(&signed, "signature").unwrap_or(signed);
        return ed25519_signature_bytes(&signature);
    }

    let message = Uint8Array::from(proof.as_bytes());
    let signed = if !js_prop(provider, "signMessage")?.is_undefined() {
        await_js(js_call2(
            provider,
            "signMessage",
            &message.into(),
            &JsValue::from_str("utf8"),
        )?)
        .await?
    } else {
        let params = Object::new();
        js_set(&params, "message", &message.into())?;
        await_js(request(provider, "signMessage", params.into())?).await?
    };
    let signature = js_prop(&signed, "signature").unwrap_or(signed);
    ed25519_signature_bytes(&signature)
}

fn ed25519_signature_bytes(signature: &JsValue) -> Result<Vec<u8>, String> {
    if let Some(value) = signature.as_string() {
        return value
            .from_base58()
            .map_err(|_| "Solana provider returned an invalid base58 signature".to_string());
    }
    if Array::is_array(signature) {
        let array = Array::from(signature);
        let mut out = Vec::with_capacity(array.length() as usize);
        for index in 0..array.length() {
            let value = array
                .get(index)
                .as_f64()
                .ok_or_else(|| "Ed25519 bridge returned a non-byte signature".to_string())?;
            if !(0.0..=255.0).contains(&value) || value.fract() != 0.0 {
                return Err("Ed25519 bridge returned a non-byte signature".to_string());
            }
            out.push(value as u8);
        }
        return Ok(out);
    }
    Ok(Uint8Array::new(signature).to_vec())
}

fn extension_wallet_bridge() -> Option<JsValue> {
    let bridge = js_global_prop(EXTENSION_WALLET_BRIDGE).ok()?;
    if bridge.is_undefined() || bridge.is_null() || !is_extension_wallet_bridge(&bridge) {
        return None;
    }
    Some(bridge)
}

fn is_extension_wallet_bridge(value: &JsValue) -> bool {
    is_callable(value, "connect") && is_callable(value, "sign")
}

async fn connect_extension_wallet(
    bridge: &JsValue,
    kind: WalletKind,
    wallet: &str,
    fallback_account_type: &str,
) -> Result<WalletAccount, String> {
    let response = await_js(js_call1(bridge, "connect", &JsValue::from_str(wallet))?).await?;
    let account = js_string_field(&response, "account")?;
    let account_type = js_string_field(&response, "accountType")
        .unwrap_or_else(|_| fallback_account_type.to_string());
    Ok(WalletAccount {
        kind,
        account,
        account_type,
        handle: bridge.clone(),
    })
}

async fn sign_extension_wallet(
    bridge: &JsValue,
    wallet: &str,
    proof: &str,
    account: Option<&str>,
) -> Result<JsValue, String> {
    await_js(js_call3(
        bridge,
        "sign",
        &JsValue::from_str(wallet),
        &JsValue::from_str(proof),
        &JsValue::from_str(account.unwrap_or_default()),
    )?)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_base64url_decoder_preserves_non_ascii_coordinate_bytes() {
        assert_eq!(base64_url_to_bytes("AH-A_w"), Ok(vec![0, 127, 128, 255]));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_rings_prefix_matches_core_secp256r1_transcript() {
        assert_eq!(
            bytes_to_hex(&rings_prefixed_message("hello world")),
            "1952696e6773205369676e6564204d6573736167653a0a313168656c6c6f20776f726c64"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_hex_signature_parser_accepts_prefixed_even_hex() {
        assert_eq!(hex_to_bytes("0x000aff"), Ok(vec![0, 10, 255]));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn test_webcrypto_signature_normalizer_rewrites_high_s() {
        let result = (|| -> Result<(), String> {
            let low_s = hex_to_bytes("43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bf4fe1343f0b08a810768475fa87fd061a586e943ca9665ee167a3f63c70c72fd9")?;
            let high_s = hex_to_bytes("43e9f1ce3f4fc0761805cb13b3ec188ccd3d509b7e563f3794e5daf84eaf43bfb01ecbbff4f757f0897b8a057802f9e564786670fdb13fa38c15d4868b9bf578")?;

            assert_eq!(normalize_webcrypto_signature(high_s), Ok(low_s));
            Ok(())
        })();

        assert_eq!(result, Ok(()));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use rings_node::prelude::rings_core::delegation::DelegationBuilder;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test(async)]
    async fn test_webcrypto_account_authorizes_delegatee_key() {
        let account = connect_webcrypto().await;
        assert!(account.is_ok());
        let Ok(account) = account else {
            return;
        };
        assert_eq!(account.account_type.as_str(), "secp256r1");

        let mut builder =
            DelegationBuilder::new(account.account.clone(), account.account_type.clone());
        let proof = builder.unsigned_proof();
        let signature = account.sign_session_proof(&proof).await;
        assert!(signature.is_ok());
        let Ok(signature) = signature else {
            return;
        };
        assert_eq!(signature.len(), 64);

        builder = builder.set_delegator_signature(signature);
        let delegatee_key = builder.build();
        assert!(delegatee_key.is_ok());
        let Ok(delegatee_key) = delegatee_key else {
            return;
        };
        assert!(delegatee_key
            .delegation()
            .verify_delegator_authorization()
            .is_ok());

        let second = connect_webcrypto().await;
        assert!(second.is_ok());
        let Ok(second) = second else {
            return;
        };
        assert_eq!(second.account, account.account);

        let mut second_builder =
            DelegationBuilder::new(second.account.clone(), second.account_type.clone());
        let second_proof = second_builder.unsigned_proof();
        let second_sig = second.sign_session_proof(&second_proof).await;
        assert!(second_sig.is_ok());
        let Ok(second_sig) = second_sig else {
            return;
        };
        second_builder = second_builder.set_delegator_signature(second_sig);
        let second_delegatee = second_builder.build();
        assert!(second_delegatee.is_ok());
        let Ok(second_delegatee) = second_delegatee else {
            return;
        };
        assert_eq!(
            second_delegatee.delegator_did(),
            delegatee_key.delegator_did()
        );
        let extractable = js_prop(&second.handle, "extractable")
            .ok()
            .and_then(|val| val.as_bool());
        assert_eq!(extractable, Some(false));

        let crypto = js_global_prop("crypto");
        assert!(crypto.is_ok());
        let Ok(crypto) = crypto else {
            return;
        };
        let subtle = js_prop(&crypto, "subtle");
        assert!(subtle.is_ok());
        let Ok(subtle) = subtle else {
            return;
        };
        let mismatched = generate_webcrypto_account(&subtle).await;
        assert!(mismatched.is_ok());
        let Ok(mismatched) = mismatched else {
            return;
        };
        let corrupted = WalletAccount {
            kind: WalletKind::WebCrypto,
            account: mismatched.account.clone(),
            account_type: "secp256r1".to_string(),
            handle: second.handle.clone(),
        };
        assert!(persist_webcrypto_account(&corrupted).await.is_ok());
        assert!(load_persisted_webcrypto_account().await.is_err());

        let recovered = connect_webcrypto().await;
        assert!(recovered.is_ok());
        let Ok(recovered) = recovered else {
            return;
        };
        assert_ne!(recovered.account, mismatched.account);
    }
}
