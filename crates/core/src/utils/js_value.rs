//! Lossless and JSON-safe conversions between Rust `serde` types and JavaScript `JsValue`s.

use js_sys::Array;
use js_sys::ArrayBuffer;
use js_sys::BigInt;
use js_sys::JsString;
use js_sys::Map;
use js_sys::Number;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use serde::de;
use serde::de::DeserializeOwned;
use serde::de::IntoDeserializer;
use serde::ser;
use serde::Serialize;
use serde::Serializer as _;
use wasm_bindgen::convert::FromWasmAbi;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;

use crate::error::Error;
use crate::error::Result;

/// Magic newtype wrapper name used by `serde_wasm_bindgen::preserve`.
const PRESERVED_VALUE_MAGIC: &str = "1fc430ca-5b7f-4295-92de-33cf2b145d38";

/// Minimum exact IEEE-754 integer (`Number.MIN_SAFE_INTEGER`, `-(2^53 - 1)`).
const MIN_SAFE_INTEGER_I64: i64 = -((1_i64 << 53) - 1);

/// Maximum exact IEEE-754 integer (`Number.MAX_SAFE_INTEGER`, `2^53 - 1`).
const MAX_SAFE_INTEGER_I64: i64 = (1_i64 << 53) - 1;

/// Maximum exact unsigned IEEE-754 integer (`Number.MAX_SAFE_INTEGER`, `2^53 - 1`).
const MAX_SAFE_INTEGER_U64: u64 = (1_u64 << 53) - 1;

/// Maximum nesting depth accepted when converting an arbitrary `JsValue` into `serde_json::Value`.
const MAX_JSON_DEPTH: usize = 128;

/// Serialize a Rust value into a JSON-compatible [`JsValue`].
///
/// Integers inside IEEE-754 safe bounds (`-(2^53 - 1)..=2^53 - 1`) serialize as JavaScript
/// `Number`s; `u64`, `i64`, `u128`, and `i128` values outside safe bounds serialize as exact
/// base-10 strings so they neither fail range checks nor emit `BigInt` values that throw under
/// `JSON.stringify`.
pub fn serialize(obj: &impl Serialize) -> Result<JsValue> {
    obj.serialize(&JsonSafeJsSerializer)
        .map_err(Error::SerdeWasmBindgenError)
}

/// Serialize a Rust value for IndexedDB structured-clone storage.
///
/// Maps serialize as plain JavaScript objects, `None` and `()` as `null`, byte buffers as compact
/// `Uint8Array`s (avoiding 8x boxed `Array` expansion), and 64/128-bit integers as exact
/// `BigInt`s supported natively by IndexedDB structured clone.
pub fn serialize_storage(obj: &impl Serialize) -> Result<JsValue> {
    let serializer = serde_wasm_bindgen::Serializer::new()
        .serialize_missing_as_null(true)
        .serialize_maps_as_objects(true)
        .serialize_large_number_types_as_bigints(true);
    serializer
        .serialize_some(obj)
        .map_err(Error::SerdeWasmBindgenError)
}

/// Deserialize a [`JsValue`] into `T`, accepting safe `Number`s, `BigInt`s, and base-10 integer
/// strings for 64-bit and 128-bit integers as well as `Uint8Array`, `ArrayBuffer`, and `Array`
/// byte sequences.
pub fn deserialize<T: DeserializeOwned>(obj: impl Into<JsValue>) -> Result<T> {
    let value = obj.into();
    serde_wasm_bindgen::from_value(value.clone())
        .or_else(|_| T::deserialize(LosslessJsDeserializer { value }))
        .map_err(Error::SerdeWasmBindgenError)
}

/// Convert a [`JsValue`] into a [`serde_json::Value`] without calling `JSON.stringify` on
/// `undefined` or `BigInt` values.
pub fn json_value(obj: impl Into<JsValue>) -> Result<serde_json::Value> {
    js_to_json_value(obj.into(), 0)
}

fn js_to_json_value(value: JsValue, depth: usize) -> Result<serde_json::Value> {
    if depth > MAX_JSON_DEPTH {
        return Err(Error::JsError(
            "JavaScript value exceeded maximum JSON nesting depth".to_string(),
        ));
    }
    if value.is_undefined() || value.is_null() {
        return Ok(serde_json::Value::Null);
    }
    if let Some(flag) = value.as_bool() {
        return Ok(serde_json::Value::Bool(flag));
    }
    if value.is_bigint() {
        if let Ok(signed) = i64::try_from(value.clone()) {
            return Ok(serde_json::Value::Number(signed.into()));
        }
        if let Ok(unsigned) = u64::try_from(value.clone()) {
            return Ok(serde_json::Value::Number(unsigned.into()));
        }
        let bigint: BigInt = value.unchecked_into();
        let text = bigint
            .to_string(10)
            .map(String::from)
            .map_err(|_| Error::JsError("failed to format BigInt".to_string()))?;
        return Ok(serde_json::Value::String(text));
    }
    if let Some(num) = value.as_f64() {
        if !num.is_finite() {
            return Err(Error::JsError(
                "non-finite JavaScript number cannot be represented in JSON".to_string(),
            ));
        }
        if num.fract() == 0.0 {
            if (MIN_SAFE_INTEGER_I64 as f64..=MAX_SAFE_INTEGER_I64 as f64).contains(&num) {
                return Ok(serde_json::Value::Number((num as i64).into()));
            }
            if num >= 0.0 && num <= u64::MAX as f64 {
                if let Some(number) = serde_json::Number::from_f64(num) {
                    return Ok(serde_json::Value::Number(number));
                }
            }
        }
        let number = serde_json::Number::from_f64(num).ok_or_else(|| {
            Error::JsError("failed to convert JavaScript number to JSON".to_string())
        })?;
        return Ok(serde_json::Value::Number(number));
    }
    if let Some(text) = value.as_string() {
        return Ok(serde_json::Value::String(text));
    }
    if let Some(bytes) = extract_bytes(&value) {
        return Ok(serde_json::Value::Array(
            bytes
                .into_iter()
                .map(|byte| serde_json::Value::Number(byte.into()))
                .collect(),
        ));
    }
    if Array::is_array(&value) {
        let array: Array = value.unchecked_into();
        let mut items = Vec::with_capacity(array.length() as usize);
        for element in array.iter() {
            let item = if element.is_undefined() {
                serde_json::Value::Null
            } else {
                js_to_json_value(element, depth + 1)?
            };
            items.push(item);
        }
        return Ok(serde_json::Value::Array(items));
    }
    if value.is_function() || value.is_symbol() {
        return Err(Error::JsError(
            "JavaScript functions and symbols cannot be converted to JSON".to_string(),
        ));
    }
    if let Some(map) = value.dyn_ref::<Map>() {
        let mut object = serde_json::Map::new();
        let entries = Array::from(&map.entries());
        for pair in entries.iter() {
            let pair = Array::from(&pair);
            let key_val = pair.get(0);
            let val = pair.get(1);
            if val.is_undefined() {
                continue;
            }
            let key = js_key_to_string(&key_val)?;
            object.insert(key, js_to_json_value(val, depth + 1)?);
        }
        return Ok(serde_json::Value::Object(object));
    }
    if value.is_object() {
        let entries = Object::entries(value.unchecked_ref());
        let mut object = serde_json::Map::with_capacity(entries.length() as usize);
        for pair in entries.iter() {
            let pair = Array::from(&pair);
            let key_val = pair.get(0);
            let val = pair.get(1);
            if val.is_undefined() || val.is_function() || val.is_symbol() {
                continue;
            }
            let key = js_key_to_string(&key_val)?;
            object.insert(key, js_to_json_value(val, depth + 1)?);
        }
        return Ok(serde_json::Value::Object(object));
    }
    Err(Error::JsError(
        "unsupported JavaScript value for JSON conversion".to_string(),
    ))
}

fn js_key_to_string(key: &JsValue) -> Result<String> {
    if let Some(text) = key.as_string() {
        return Ok(text);
    }
    if let Some(num) = key.as_f64() {
        if num.is_finite() {
            return Ok(num.to_string());
        }
    }
    if key.is_bigint() {
        let bigint: &BigInt = key.unchecked_ref();
        return bigint
            .to_string(10)
            .map(String::from)
            .map_err(|_| Error::JsError("failed to stringify BigInt map key".to_string()));
    }
    Err(Error::JsError(
        "JavaScript map key is not convertible to a JSON object key".to_string(),
    ))
}

fn extract_bytes(value: &JsValue) -> Option<Vec<u8>> {
    if let Some(view) = value.dyn_ref::<Uint8Array>() {
        return Some(view.to_vec());
    }
    if let Some(buffer) = value.dyn_ref::<ArrayBuffer>() {
        return Some(Uint8Array::new(buffer).to_vec());
    }
    None
}

struct JsonSafeJsSerializer;

struct JsArraySeq {
    target: Array,
    idx: u32,
}

impl JsArraySeq {
    fn new() -> Self {
        Self {
            target: Array::new(),
            idx: 0,
        }
    }
}

impl ser::SerializeSeq for JsArraySeq {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_element<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        let js = value.serialize(&JsonSafeJsSerializer)?;
        self.target.set(self.idx, js);
        self.idx = self.idx.saturating_add(1);
        Ok(())
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(self.target.into())
    }
}

impl ser::SerializeTuple for JsArraySeq {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_element<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        ser::SerializeSeq::end(self)
    }
}

impl ser::SerializeTupleStruct for JsArraySeq {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        ser::SerializeSeq::serialize_element(self, value)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        ser::SerializeSeq::end(self)
    }
}

struct JsObjectMap {
    target: Object,
    next_key: Option<JsString>,
}

impl JsObjectMap {
    fn new() -> Self {
        Self {
            target: Object::new(),
            next_key: None,
        }
    }
}

fn js_err(msg: impl std::fmt::Display) -> serde_wasm_bindgen::Error {
    serde_wasm_bindgen::Error::new(msg)
}

impl ser::SerializeMap for JsObjectMap {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_key<T: ?Sized + Serialize>(
        &mut self,
        key: &T,
    ) -> std::result::Result<(), Self::Error> {
        let key_val = key.serialize(&JsonSafeJsSerializer)?;
        let key_str = js_key_to_string(&key_val)
            .map(JsString::from)
            .map_err(js_err)?;
        self.next_key = Some(key_str);
        Ok(())
    }

    fn serialize_value<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        let key = self
            .next_key
            .take()
            .ok_or_else(|| js_err("serialize_value called before serialize_key"))?;
        let val = value.serialize(&JsonSafeJsSerializer)?;
        Reflect::set(&self.target, key.as_ref(), &val).map_err(|err| js_err(format!("{err:?}")))?;
        Ok(())
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(self.target.into())
    }
}

struct JsStructSerializer {
    target: Object,
}

impl JsStructSerializer {
    fn new() -> Self {
        Self {
            target: Object::new(),
        }
    }
}

impl ser::SerializeStruct for JsStructSerializer {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        let val = value.serialize(&JsonSafeJsSerializer)?;
        Reflect::set(&self.target, &JsValue::from_str(key), &val)
            .map_err(|err| js_err(format!("{err:?}")))?;
        Ok(())
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(self.target.into())
    }
}

struct JsVariantSerializer<S> {
    variant: &'static str,
    inner: S,
}

impl<S> JsVariantSerializer<S> {
    fn wrap(
        variant: &'static str,
        value: JsValue,
    ) -> std::result::Result<JsValue, serde_wasm_bindgen::Error> {
        let outer = Object::new();
        Reflect::set(&outer, &JsValue::from_str(variant), &value)
            .map_err(|err| js_err(format!("{err:?}")))?;
        Ok(outer.into())
    }
}

impl ser::SerializeTupleVariant for JsVariantSerializer<JsArraySeq> {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        ser::SerializeSeq::serialize_element(&mut self.inner, value)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        let inner = ser::SerializeSeq::end(self.inner)?;
        Self::wrap(self.variant, inner)
    }
}

impl ser::SerializeStructVariant for JsVariantSerializer<JsStructSerializer> {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;

    fn serialize_field<T: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> std::result::Result<(), Self::Error> {
        ser::SerializeStruct::serialize_field(&mut self.inner, key, value)
    }

    fn end(self) -> std::result::Result<Self::Ok, Self::Error> {
        let inner = ser::SerializeStruct::end(self.inner)?;
        Self::wrap(self.variant, inner)
    }
}

struct PreservedAbiCapture;

impl ser::Serializer for PreservedAbiCapture {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;
    type SerializeSeq = ser::Impossible<JsValue, Self::Error>;
    type SerializeTuple = ser::Impossible<JsValue, Self::Error>;
    type SerializeTupleStruct = ser::Impossible<JsValue, Self::Error>;
    type SerializeTupleVariant = ser::Impossible<JsValue, Self::Error>;
    type SerializeMap = ser::Impossible<JsValue, Self::Error>;
    type SerializeStruct = ser::Impossible<JsValue, Self::Error>;
    type SerializeStructVariant = ser::Impossible<JsValue, Self::Error>;

    fn serialize_u32(self, v: u32) -> std::result::Result<Self::Ok, Self::Error> {
        let val: JsValue = unsafe { FromWasmAbi::from_abi(v) };
        let cloned = val.clone();
        std::mem::forget(val);
        Ok(cloned)
    }

    fn serialize_bool(self, _: bool) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected bool in preserved JsValue"))
    }
    fn serialize_i8(self, _: i8) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected i8 in preserved JsValue"))
    }
    fn serialize_i16(self, _: i16) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected i16 in preserved JsValue"))
    }
    fn serialize_i32(self, _: i32) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected i32 in preserved JsValue"))
    }
    fn serialize_i64(self, _: i64) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected i64 in preserved JsValue"))
    }
    fn serialize_u8(self, _: u8) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected u8 in preserved JsValue"))
    }
    fn serialize_u16(self, _: u16) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected u16 in preserved JsValue"))
    }
    fn serialize_u64(self, _: u64) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected u64 in preserved JsValue"))
    }
    fn serialize_f32(self, _: f32) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected f32 in preserved JsValue"))
    }
    fn serialize_f64(self, _: f64) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected f64 in preserved JsValue"))
    }
    fn serialize_char(self, _: char) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected char in preserved JsValue"))
    }
    fn serialize_str(self, _: &str) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected str in preserved JsValue"))
    }
    fn serialize_bytes(self, _: &[u8]) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected bytes in preserved JsValue"))
    }
    fn serialize_none(self) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected none in preserved JsValue"))
    }
    fn serialize_some<T: ?Sized + Serialize>(
        self,
        _: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected some in preserved JsValue"))
    }
    fn serialize_unit(self) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom("unexpected unit in preserved JsValue"))
    }
    fn serialize_unit_struct(self, _: &'static str) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom(
            "unexpected unit_struct in preserved JsValue",
        ))
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom(
            "unexpected unit_variant in preserved JsValue",
        ))
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom(
            "unexpected newtype_struct in preserved JsValue",
        ))
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Err(ser::Error::custom(
            "unexpected newtype_variant in preserved JsValue",
        ))
    }
    fn serialize_seq(
        self,
        _: Option<usize>,
    ) -> std::result::Result<Self::SerializeSeq, Self::Error> {
        Err(ser::Error::custom("unexpected seq in preserved JsValue"))
    }
    fn serialize_tuple(self, _: usize) -> std::result::Result<Self::SerializeTuple, Self::Error> {
        Err(ser::Error::custom("unexpected tuple in preserved JsValue"))
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> std::result::Result<Self::SerializeTupleStruct, Self::Error> {
        Err(ser::Error::custom(
            "unexpected tuple_struct in preserved JsValue",
        ))
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> std::result::Result<Self::SerializeTupleVariant, Self::Error> {
        Err(ser::Error::custom(
            "unexpected tuple_variant in preserved JsValue",
        ))
    }
    fn serialize_map(
        self,
        _: Option<usize>,
    ) -> std::result::Result<Self::SerializeMap, Self::Error> {
        Err(ser::Error::custom("unexpected map in preserved JsValue"))
    }
    fn serialize_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> std::result::Result<Self::SerializeStruct, Self::Error> {
        Err(ser::Error::custom("unexpected struct in preserved JsValue"))
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        _: &'static str,
        _: usize,
    ) -> std::result::Result<Self::SerializeStructVariant, Self::Error> {
        Err(ser::Error::custom(
            "unexpected struct_variant in preserved JsValue",
        ))
    }
}

impl ser::Serializer for &JsonSafeJsSerializer {
    type Ok = JsValue;
    type Error = serde_wasm_bindgen::Error;
    type SerializeSeq = JsArraySeq;
    type SerializeTuple = JsArraySeq;
    type SerializeTupleStruct = JsArraySeq;
    type SerializeTupleVariant = JsVariantSerializer<JsArraySeq>;
    type SerializeMap = JsObjectMap;
    type SerializeStruct = JsStructSerializer;
    type SerializeStructVariant = JsVariantSerializer<JsStructSerializer>;

    fn serialize_bool(self, v: bool) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_bool(v))
    }

    fn serialize_i8(self, v: i8) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_i16(self, v: i16) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_i32(self, v: i32) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_i64(self, v: i64) -> std::result::Result<Self::Ok, Self::Error> {
        if (MIN_SAFE_INTEGER_I64..=MAX_SAFE_INTEGER_I64).contains(&v) {
            Ok(JsValue::from_f64(v as f64))
        } else {
            Ok(JsValue::from_str(&v.to_string()))
        }
    }

    fn serialize_i128(self, v: i128) -> std::result::Result<Self::Ok, Self::Error> {
        if (i128::from(MIN_SAFE_INTEGER_I64)..=i128::from(MAX_SAFE_INTEGER_I64)).contains(&v) {
            Ok(JsValue::from_f64(v as f64))
        } else {
            Ok(JsValue::from_str(&v.to_string()))
        }
    }

    fn serialize_u8(self, v: u8) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_u16(self, v: u16) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_u32(self, v: u32) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_f64(f64::from(v)))
    }

    fn serialize_u64(self, v: u64) -> std::result::Result<Self::Ok, Self::Error> {
        if v <= MAX_SAFE_INTEGER_U64 {
            Ok(JsValue::from_f64(v as f64))
        } else {
            Ok(JsValue::from_str(&v.to_string()))
        }
    }

    fn serialize_u128(self, v: u128) -> std::result::Result<Self::Ok, Self::Error> {
        if v <= u128::from(MAX_SAFE_INTEGER_U64) {
            Ok(JsValue::from_f64(v as f64))
        } else {
            Ok(JsValue::from_str(&v.to_string()))
        }
    }

    fn serialize_f32(self, v: f32) -> std::result::Result<Self::Ok, Self::Error> {
        self.serialize_f64(f64::from(v))
    }

    fn serialize_f64(self, v: f64) -> std::result::Result<Self::Ok, Self::Error> {
        if v.is_finite() {
            Ok(JsValue::from_f64(v))
        } else {
            Ok(JsValue::NULL)
        }
    }

    fn serialize_char(self, v: char) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsString::from(v).into())
    }

    fn serialize_str(self, v: &str) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_str(v))
    }

    fn serialize_bytes(self, v: &[u8]) -> std::result::Result<Self::Ok, Self::Error> {
        let array = Array::new_with_length(u32::try_from(v.len()).map_err(js_err)?);
        for (idx, byte) in v.iter().copied().enumerate() {
            array.set(idx as u32, JsValue::from_f64(f64::from(byte)));
        }
        Ok(array.into())
    }

    fn serialize_none(self) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::NULL)
    }

    fn serialize_some<T: ?Sized + Serialize>(
        self,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::NULL)
    }

    fn serialize_unit_struct(
        self,
        _name: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::NULL)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        Ok(JsValue::from_str(variant))
    }

    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        if name == PRESERVED_VALUE_MAGIC {
            return value.serialize(PreservedAbiCapture);
        }
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> std::result::Result<Self::Ok, Self::Error> {
        let inner = value.serialize(self)?;
        JsVariantSerializer::<()>::wrap(variant, inner)
    }

    fn serialize_seq(
        self,
        _len: Option<usize>,
    ) -> std::result::Result<Self::SerializeSeq, Self::Error> {
        Ok(JsArraySeq::new())
    }

    fn serialize_tuple(
        self,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTuple, Self::Error> {
        Ok(JsArraySeq::new())
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(JsArraySeq::new())
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(JsVariantSerializer {
            variant,
            inner: JsArraySeq::new(),
        })
    }

    fn serialize_map(
        self,
        _len: Option<usize>,
    ) -> std::result::Result<Self::SerializeMap, Self::Error> {
        Ok(JsObjectMap::new())
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeStruct, Self::Error> {
        Ok(JsStructSerializer::new())
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        _len: usize,
    ) -> std::result::Result<Self::SerializeStructVariant, Self::Error> {
        Ok(JsVariantSerializer {
            variant,
            inner: JsStructSerializer::new(),
        })
    }
}

struct LosslessJsDeserializer {
    value: JsValue,
}

impl LosslessJsDeserializer {
    fn safe_i64(&self) -> Option<i64> {
        if Number::is_safe_integer(&self.value) {
            self.value.as_f64().map(|v| v as i64)
        } else {
            None
        }
    }
}

struct JsSeqAccess {
    items: std::vec::IntoIter<JsValue>,
}

impl<'de> de::SeqAccess<'de> for JsSeqAccess {
    type Error = serde_wasm_bindgen::Error;

    fn next_element_seed<T>(
        &mut self,
        seed: T,
    ) -> std::result::Result<Option<T::Value>, Self::Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        self.items
            .next()
            .map(|value| seed.deserialize(LosslessJsDeserializer { value }))
            .transpose()
    }
}

struct JsMapAccess {
    entries: std::vec::IntoIter<(JsValue, JsValue)>,
    pending_value: Option<JsValue>,
}

impl<'de> de::MapAccess<'de> for JsMapAccess {
    type Error = serde_wasm_bindgen::Error;

    fn next_key_seed<K>(&mut self, seed: K) -> std::result::Result<Option<K::Value>, Self::Error>
    where K: de::DeserializeSeed<'de> {
        let Some((key, value)) = self.entries.next() else {
            return Ok(None);
        };
        self.pending_value = Some(value);
        seed.deserialize(LosslessJsDeserializer { value: key })
            .map(Some)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> std::result::Result<V::Value, Self::Error>
    where V: de::DeserializeSeed<'de> {
        let value = self
            .pending_value
            .take()
            .ok_or_else(|| js_err("next_value_seed called without pending key"))?;
        seed.deserialize(LosslessJsDeserializer { value })
    }
}

struct JsEnumAccess {
    tag: String,
    payload: JsValue,
}

impl<'de> de::EnumAccess<'de> for JsEnumAccess {
    type Error = serde_wasm_bindgen::Error;
    type Variant = Self;

    fn variant_seed<V>(
        self,
        seed: V,
    ) -> std::result::Result<(V::Value, Self::Variant), Self::Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let tag_de: de::value::StrDeserializer<'_, Self::Error> =
            self.tag.as_str().into_deserializer();
        let variant = seed.deserialize(tag_de)?;
        Ok((variant, self))
    }
}

impl<'de> de::VariantAccess<'de> for JsEnumAccess {
    type Error = serde_wasm_bindgen::Error;

    fn unit_variant(self) -> std::result::Result<(), Self::Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> std::result::Result<T::Value, Self::Error>
    where T: de::DeserializeSeed<'de> {
        seed.deserialize(LosslessJsDeserializer {
            value: self.payload,
        })
    }

    fn tuple_variant<V>(
        self,
        len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        de::Deserializer::deserialize_tuple(
            LosslessJsDeserializer {
                value: self.payload,
            },
            len,
            visitor,
        )
    }

    fn struct_variant<V>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error>
    where
        V: de::Visitor<'de>,
    {
        de::Deserializer::deserialize_struct(
            LosslessJsDeserializer {
                value: self.payload,
            },
            "",
            fields,
            visitor,
        )
    }
}

impl<'de> de::Deserializer<'de> for LosslessJsDeserializer {
    type Error = serde_wasm_bindgen::Error;

    fn deserialize_any<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_any(visitor)
    }

    fn deserialize_bool<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_bool(visitor)
    }

    fn deserialize_i8<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_i8(visitor)
    }

    fn deserialize_i16<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_i16(visitor)
    }

    fn deserialize_i32<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_i32(visitor)
    }

    fn deserialize_i64<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(text) = self.value.as_string() {
            let parsed = text.parse::<i64>().map_err(js_err)?;
            return visitor.visit_i64(parsed);
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_i64(visitor)
    }

    fn deserialize_i128<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(safe) = self.safe_i64() {
            return visitor.visit_i128(i128::from(safe));
        }
        if let Some(text) = self.value.as_string() {
            let parsed = text.parse::<i128>().map_err(js_err)?;
            return visitor.visit_i128(parsed);
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_i128(visitor)
    }

    fn deserialize_u8<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_u8(visitor)
    }

    fn deserialize_u16<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_u16(visitor)
    }

    fn deserialize_u32<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_u32(visitor)
    }

    fn deserialize_u64<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(text) = self.value.as_string() {
            let parsed = text.parse::<u64>().map_err(js_err)?;
            return visitor.visit_u64(parsed);
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_u64(visitor)
    }

    fn deserialize_u128<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(safe) = self.safe_i64() {
            if safe >= 0 {
                return visitor.visit_u128(safe as u128);
            }
        }
        if let Some(text) = self.value.as_string() {
            let parsed = text.parse::<u128>().map_err(js_err)?;
            return visitor.visit_u128(parsed);
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_u128(visitor)
    }

    fn deserialize_f32<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_f32(visitor)
    }

    fn deserialize_f64<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_f64(visitor)
    }

    fn deserialize_char<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_char(visitor)
    }

    fn deserialize_str<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_str(visitor)
    }

    fn deserialize_string<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_string(visitor)
    }

    fn deserialize_bytes<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_bytes(visitor)
    }

    fn deserialize_byte_buf<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_byte_buf(visitor)
    }

    fn deserialize_option<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if self.value.is_undefined() || self.value.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_unit(visitor)
    }

    fn deserialize_unit_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_unit_struct(name, visitor)
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(arr) = self.value.dyn_ref::<Array>() {
            return visitor.visit_seq(JsSeqAccess {
                items: arr.to_vec().into_iter(),
            });
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_seq(visitor)
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if name == PRESERVED_VALUE_MAGIC {
            return serde_wasm_bindgen::Deserializer::from(self.value)
                .deserialize_tuple_struct(name, len, visitor);
        }
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(map) = self.value.dyn_ref::<Map>() {
            let raw = Array::from(&map.entries());
            let mut entries = Vec::with_capacity(raw.length() as usize);
            for pair in raw.iter() {
                let pair = Array::from(&pair);
                entries.push((pair.get(0), pair.get(1)));
            }
            return visitor.visit_map(JsMapAccess {
                entries: entries.into_iter(),
                pending_value: None,
            });
        }
        if self.value.is_object() {
            let raw = Object::entries(self.value.unchecked_ref());
            let mut entries = Vec::with_capacity(raw.length() as usize);
            for pair in raw.iter() {
                let pair = Array::from(&pair);
                entries.push((pair.get(0), pair.get(1)));
            }
            return visitor.visit_map(JsMapAccess {
                entries: entries.into_iter(),
                pending_value: None,
            });
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_map(visitor)
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        self.deserialize_map(visitor)
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        if let Some(tag) = self.value.as_string() {
            return visitor.visit_enum(JsEnumAccess {
                tag,
                payload: JsValue::UNDEFINED,
            });
        }
        if self.value.is_object() {
            let entries = Object::entries(self.value.unchecked_ref());
            if entries.length() == 1 {
                let pair = Array::from(&entries.get(0));
                if let Some(tag) = pair.get(0).as_string() {
                    return visitor.visit_enum(JsEnumAccess {
                        tag,
                        payload: pair.get(1),
                    });
                }
            }
        }
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_enum(name, variants, visitor)
    }

    fn deserialize_identifier<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        serde_wasm_bindgen::Deserializer::from(self.value).deserialize_identifier(visitor)
    }

    fn deserialize_ignored_any<V: de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> std::result::Result<V::Value, Self::Error> {
        visitor.visit_unit()
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde::Serialize;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::wasm_bindgen_test;

    use super::deserialize;
    use super::json_value;
    use super::serialize;
    use super::serialize_storage;

    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct LargeNumbersDto {
        safe: u64,
        above_safe: u64,
        epoch_ms: u128,
        signed_min: i128,
    }

    #[wasm_bindgen_test]
    fn json_safe_and_storage_roundtrip_preserve_large_integers() {
        let dto = LargeNumbersDto {
            safe: MAX_SAFE_INTEGER_U64,
            above_safe: u64::MAX,
            epoch_ms: u128::MAX - 7,
            signed_min: i128::MIN + 3,
        };

        let js_dto = serialize(&dto).expect("json-safe serialize");
        let json = json_value(js_dto.clone()).expect("json_value conversion");
        assert_eq!(
            json["above_safe"].as_str(),
            Some(u64::MAX.to_string().as_str())
        );
        assert_eq!(
            json["epoch_ms"].as_str(),
            Some((u128::MAX - 7).to_string().as_str())
        );
        let decoded_dto: LargeNumbersDto = deserialize(js_dto).expect("deserialize json-safe");
        assert_eq!(decoded_dto, dto);

        let storage_js = serialize_storage(&dto).expect("storage serialize");
        let decoded_storage: LargeNumbersDto =
            deserialize(storage_js).expect("deserialize storage bigint");
        assert_eq!(decoded_storage, dto);
        assert_eq!(
            json_value(JsValue::UNDEFINED).unwrap(),
            serde_json::Value::Null
        );
    }

    const MAX_SAFE_INTEGER_U64: u64 = super::MAX_SAFE_INTEGER_U64;
}
