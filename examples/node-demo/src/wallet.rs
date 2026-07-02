//! Browser account standards used to authorize a Rings session key.

use base58::FromBase58;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

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

fn js_err(error: JsValue) -> String {
    if let Some(message) = error.as_string() {
        return message;
    }
    format!("{error:?}")
}

fn global_prop(name: &str) -> Result<JsValue, String> {
    Reflect::get(&js_sys::global(), &JsValue::from_str(name)).map_err(js_err)
}

fn prop(object: &JsValue, name: &str) -> Result<JsValue, String> {
    Reflect::get(object, &JsValue::from_str(name)).map_err(js_err)
}

fn string_prop(object: &JsValue, name: &str) -> Result<String, String> {
    prop(object, name)?
        .as_string()
        .ok_or_else(|| format!("missing string field {name}"))
}

fn set(object: &Object, name: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(name), value)
        .map(|_| ())
        .map_err(js_err)
}

fn method(object: &JsValue, name: &str) -> Result<Function, String> {
    prop(object, name)?
        .dyn_into::<Function>()
        .map_err(|_| format!("{name} is not callable"))
}

async fn await_js(value: JsValue) -> Result<JsValue, String> {
    JsFuture::from(Promise::from(value)).await.map_err(js_err)
}

fn call0(object: &JsValue, name: &str) -> Result<JsValue, String> {
    method(object, name)?
        .call0(object)
        .map_err(|error| format!("{name} failed: {}", js_err(error)))
}

fn call1(object: &JsValue, name: &str, a: &JsValue) -> Result<JsValue, String> {
    method(object, name)?
        .call1(object, a)
        .map_err(|error| format!("{name} failed: {}", js_err(error)))
}

fn call2(object: &JsValue, name: &str, a: &JsValue, b: &JsValue) -> Result<JsValue, String> {
    method(object, name)?
        .call2(object, a, b)
        .map_err(|error| format!("{name} failed: {}", js_err(error)))
}

fn call3(
    object: &JsValue,
    name: &str,
    a: &JsValue,
    b: &JsValue,
    c: &JsValue,
) -> Result<JsValue, String> {
    method(object, name)?
        .call3(object, a, b, c)
        .map_err(|error| format!("{name} failed: {}", js_err(error)))
}

fn request(provider: &JsValue, method_name: &str, params: JsValue) -> Result<JsValue, String> {
    let body = Object::new();
    set(&body, "method", &JsValue::from_str(method_name))?;
    set(&body, "params", &params)?;
    call1(provider, "request", &body.into())
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
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("wallet returned an invalid hex signature".to_string()),
    }
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
    set(&algorithm, "name", &JsValue::from_str("ECDSA"))?;
    set(&algorithm, "namedCurve", &JsValue::from_str("P-256"))?;
    Ok(algorithm)
}

async fn connect_webcrypto() -> Result<WalletAccount, String> {
    let crypto = global_prop("crypto")?;
    let subtle = prop(&crypto, "subtle")?;
    if subtle.is_undefined() || subtle.is_null() {
        return Err("WebCrypto SubtleCrypto is not available".to_string());
    }

    let usages = Array::new();
    usages.push(&JsValue::from_str("sign"));
    usages.push(&JsValue::from_str("verify"));
    let key_pair = await_js(call3(
        &subtle,
        "generateKey",
        &ecdsa_algorithm()?.into(),
        &JsValue::TRUE,
        &usages.into(),
    )?)
    .await?;
    let public_key = prop(&key_pair, "publicKey")?;
    let private_key = prop(&key_pair, "privateKey")?;
    let jwk = await_js(call2(
        &subtle,
        "exportKey",
        &JsValue::from_str("jwk"),
        &public_key,
    )?)
    .await?;
    let x = base64_url_to_bytes(&string_prop(&jwk, "x")?)?;
    let y = base64_url_to_bytes(&string_prop(&jwk, "y")?)?;
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
    let crypto = global_prop("crypto")?;
    let subtle = prop(&crypto, "subtle")?;
    let hash = Object::new();
    set(&hash, "name", &JsValue::from_str("SHA-256"))?;
    let algorithm = Object::new();
    set(&algorithm, "name", &JsValue::from_str("ECDSA"))?;
    set(&algorithm, "hash", &hash.into())?;
    let message = Uint8Array::from(rings_prefixed_message(proof).as_slice());
    let signature = await_js(call3(
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

    let ethereum = global_prop("ethereum")?;
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
        let signature = prop(&signed, "signature")
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
    let phantom = global_prop("phantom").unwrap_or(JsValue::UNDEFINED);
    let nested = if phantom.is_undefined() || phantom.is_null() {
        JsValue::UNDEFINED
    } else {
        prop(&phantom, "solana").unwrap_or(JsValue::UNDEFINED)
    };
    if !nested.is_undefined() && !nested.is_null() {
        return Ok(nested);
    }
    let solana = global_prop("solana")?;
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
    await_js(call0(&provider, "connect")?).await?;
    let public_key = prop(&provider, "publicKey")?;
    let account = call0(&public_key, "toBase58")?
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
        let signature = prop(&signed, "signature").unwrap_or(signed);
        return ed25519_signature_bytes(&signature);
    }

    let message = Uint8Array::from(proof.as_bytes());
    let signed = if !prop(provider, "signMessage")?.is_undefined() {
        await_js(call2(
            provider,
            "signMessage",
            &message.into(),
            &JsValue::from_str("utf8"),
        )?)
        .await?
    } else {
        let params = Object::new();
        set(&params, "message", &message.into())?;
        await_js(request(provider, "signMessage", params.into())?).await?
    };
    let signature = prop(&signed, "signature").unwrap_or(signed);
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
    let bridge = global_prop(EXTENSION_WALLET_BRIDGE).ok()?;
    if bridge.is_undefined() || bridge.is_null() || !is_extension_wallet_bridge(&bridge) {
        return None;
    }
    Some(bridge)
}

fn is_extension_wallet_bridge(value: &JsValue) -> bool {
    is_callable(value, "connect") && is_callable(value, "sign")
}

fn is_callable(object: &JsValue, name: &str) -> bool {
    prop(object, name)
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
        .is_some()
}

async fn connect_extension_wallet(
    bridge: &JsValue,
    kind: WalletKind,
    wallet: &str,
    fallback_account_type: &str,
) -> Result<WalletAccount, String> {
    let response = await_js(call1(bridge, "connect", &JsValue::from_str(wallet))?).await?;
    let account = string_prop(&response, "account")?;
    let account_type =
        string_prop(&response, "accountType").unwrap_or_else(|_| fallback_account_type.to_string());
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
    await_js(call3(
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

    #[test]
    fn base64url_decoder_preserves_non_ascii_coordinate_bytes() {
        assert_eq!(base64_url_to_bytes("AH-A_w"), Ok(vec![0, 127, 128, 255]));
    }

    #[test]
    fn rings_prefix_matches_core_secp256r1_transcript() {
        assert_eq!(
            bytes_to_hex(&rings_prefixed_message("hello world")),
            "1952696e6773205369676e6564204d6573736167653a0a313168656c6c6f20776f726c64"
        );
    }

    #[test]
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

    #[wasm_bindgen_test]
    fn base64url_decoder_preserves_webcrypto_coordinate_bytes() {
        assert_eq!(base64_url_to_bytes("AH-A_w"), Ok(vec![0, 127, 128, 255]));
    }

    #[wasm_bindgen_test(async)]
    async fn webcrypto_account_authorizes_session_key() {
        let account = connect_webcrypto().await.expect("webcrypto account");
        assert_eq!(account.account_type.as_str(), "secp256r1");

        let mut builder =
            SessionSkBuilder::new(account.account.clone(), account.account_type.clone());
        let proof = builder.unsigned_proof();
        let signature = account
            .sign_session_proof(&proof)
            .await
            .expect("webcrypto signature");
        assert_eq!(signature.len(), 64);

        builder = builder.set_session_sig(signature);
        let session_key = builder.build().expect("valid webcrypto session key");
        assert!(session_key.session().verify_self().is_ok());
    }
}
