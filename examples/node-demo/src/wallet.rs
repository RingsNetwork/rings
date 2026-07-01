//! Browser account providers used to authorize a Rings session key.

use base58::FromBase58;
use js_sys::Array;
use js_sys::Function;
use js_sys::Object;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

/// Wallet provider selected by the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalletKind {
    /// Browser-native P-256 key generated with WebCrypto.
    WebCrypto,
    /// EIP-191 signature through `window.ethereum`.
    MetaMask,
    /// Ed25519 signature through Phantom's Solana provider.
    Phantom,
}

/// Connected browser account and the opaque signing handle.
#[derive(Clone)]
pub struct WalletAccount {
    /// Provider kind that created this account.
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
            "metamask" => Self::MetaMask,
            "phantom" => Self::Phantom,
            _ => Self::WebCrypto,
        }
    }

    /// UI value.
    pub fn value(self) -> &'static str {
        match self {
            Self::WebCrypto => "webcrypto",
            Self::MetaMask => "metamask",
            Self::Phantom => "phantom",
        }
    }

    /// Human label.
    pub fn label(self) -> &'static str {
        match self {
            Self::WebCrypto => "WebCrypto P-256",
            Self::MetaMask => "MetaMask EIP-191",
            Self::Phantom => "Phantom Ed25519",
        }
    }
}

impl WalletAccount {
    /// Sign the session proof expected by `SessionSkBuilder`.
    pub async fn sign_session_proof(&self, proof: &str) -> Result<Vec<u8>, String> {
        match self.kind {
            WalletKind::WebCrypto => sign_webcrypto(&self.handle, proof).await,
            WalletKind::MetaMask => sign_metamask(&self.handle, &self.account, proof).await,
            WalletKind::Phantom => sign_phantom(&self.handle, proof).await,
        }
    }
}

/// Connect the selected provider and return its Rings account metadata.
pub async fn connect(kind: WalletKind) -> Result<WalletAccount, String> {
    match kind {
        WalletKind::WebCrypto => connect_webcrypto().await,
        WalletKind::MetaMask => connect_metamask().await,
        WalletKind::Phantom => connect_phantom().await,
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
    if clean.len() % 2 != 0 || !clean.bytes().all(|byte| byte.is_ascii_hexdigit()) {
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
    let normalized = value.replace('-', "+").replace('_', "/");
    let padded = match normalized.len() % 4 {
        0 => normalized,
        rem => format!("{}{}", normalized, "=".repeat(4 - rem)),
    };
    let atob = method(&js_sys::global(), "atob")?;
    let binary = atob
        .call1(&JsValue::NULL, &JsValue::from_str(&padded))
        .map_err(js_err)?
        .as_string()
        .ok_or_else(|| "atob returned a non-string".to_string())?;
    Ok(binary.bytes().collect())
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

async fn connect_metamask() -> Result<WalletAccount, String> {
    let ethereum = global_prop("ethereum")?;
    if ethereum.is_undefined() || ethereum.is_null() {
        return Err("MetaMask provider not found".to_string());
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
        .ok_or_else(|| "MetaMask returned no account".to_string())?;
    Ok(WalletAccount {
        kind: WalletKind::MetaMask,
        account,
        account_type: "eip191".to_string(),
        handle: ethereum,
    })
}

async fn sign_metamask(ethereum: &JsValue, account: &str, proof: &str) -> Result<Vec<u8>, String> {
    let params = Array::new();
    params.push(&JsValue::from_str(proof));
    params.push(&JsValue::from_str(account));
    let signature = await_js(request(ethereum, "personal_sign", params.into())?).await?;
    let signature = signature
        .as_string()
        .ok_or_else(|| "MetaMask returned a non-string signature".to_string())?;
    hex_to_bytes(&signature)
}

fn phantom_provider() -> Result<JsValue, String> {
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
        Err("Phantom provider not found".to_string())
    } else {
        Ok(solana)
    }
}

async fn connect_phantom() -> Result<WalletAccount, String> {
    let provider = phantom_provider()?;
    await_js(call0(&provider, "connect")?).await?;
    let public_key = prop(&provider, "publicKey")?;
    let account = call0(&public_key, "toBase58")?
        .as_string()
        .ok_or_else(|| "Phantom returned no public key".to_string())?;
    Ok(WalletAccount {
        kind: WalletKind::Phantom,
        account,
        account_type: "ed25519".to_string(),
        handle: provider,
    })
}

async fn sign_phantom(provider: &JsValue, proof: &str) -> Result<Vec<u8>, String> {
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
    phantom_signature_bytes(&signature)
}

fn phantom_signature_bytes(signature: &JsValue) -> Result<Vec<u8>, String> {
    if let Some(value) = signature.as_string() {
        return value
            .from_base58()
            .map_err(|_| "Phantom returned an invalid base58 signature".to_string());
    }
    Ok(Uint8Array::new(signature).to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

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
