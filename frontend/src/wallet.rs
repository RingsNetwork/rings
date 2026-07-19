//! Browser account standards used to authorize a Rings session key.

use base58::FromBase58;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use js_sys::Array;
use js_sys::Object;
use js_sys::Uint8Array;
use wasm_bindgen::JsValue;

use crate::browser_api::await_js;
use crate::browser_api::is_callable;
use crate::browser_api::js_call0;
use crate::browser_api::js_call1;
use crate::browser_api::js_call2;
use crate::browser_api::js_call3;
use crate::browser_api::js_global_prop;
use crate::browser_api::js_prop;
use crate::browser_api::js_set;
use crate::browser_api::js_string_field;
use crate::hex;

const EXTENSION_WALLET_BRIDGE: &str = "RingsExtensionWalletBridge";

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
    /// Account entity passed to `SessionSkBuilder`.
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

    /// Sign the session proof expected by `SessionSkBuilder`.
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

    let usages = Array::new();
    usages.push(&JsValue::from_str("sign"));
    usages.push(&JsValue::from_str("verify"));
    let key_pair = await_js(js_call3(
        &subtle,
        "generateKey",
        &ecdsa_algorithm()?.into(),
        &JsValue::TRUE,
        &usages.into(),
    )?)
    .await?;
    let public_key = js_prop(&key_pair, "publicKey")?;
    let private_key = js_prop(&key_pair, "privateKey")?;
    let jwk = await_js(js_call2(
        &subtle,
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
    Ok(Uint8Array::new(&signature).to_vec())
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
    fn base64url_decoder_preserves_non_ascii_coordinate_bytes() {
        assert_eq!(base64_url_to_bytes("AH-A_w"), Ok(vec![0, 127, 128, 255]));
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn rings_prefix_matches_core_secp256r1_transcript() {
        assert_eq!(
            bytes_to_hex(&rings_prefixed_message("hello world")),
            "1952696e6773205369676e6564204d6573736167653a0a313168656c6c6f20776f726c64"
        );
    }

    #[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
    #[cfg_attr(not(target_arch = "wasm32"), test)]
    fn hex_signature_parser_accepts_prefixed_even_hex() {
        assert_eq!(hex_to_bytes("0x000aff"), Ok(vec![0, 10, 255]));
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use rings_node::prelude::rings_core::session::SessionSkBuilder;
    use wasm_bindgen_test::wasm_bindgen_test;
    use wasm_bindgen_test::wasm_bindgen_test_configure;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test(async)]
    async fn webcrypto_account_authorizes_session_key() {
        let account = connect_webcrypto().await;
        assert!(account.is_ok());
        let Ok(account) = account else {
            return;
        };
        assert_eq!(account.account_type.as_str(), "secp256r1");

        let mut builder =
            SessionSkBuilder::new(account.account.clone(), account.account_type.clone());
        let proof = builder.unsigned_proof();
        let signature = account.sign_session_proof(&proof).await;
        assert!(signature.is_ok());
        let Ok(signature) = signature else {
            return;
        };
        assert_eq!(signature.len(), 64);

        builder = builder.set_session_sig(signature);
        let session_key = builder.build();
        assert!(session_key.is_ok());
        let Ok(session_key) = session_key else {
            return;
        };
        assert!(session_key.session().verify_self().is_ok());
    }
}
