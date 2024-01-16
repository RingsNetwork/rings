use serde::Serializer;
use serde::Deserializer;
use serde::Deserialize;
use serde::Serialize;

pub(crate) fn serialize_forward<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    value.serialize(serializer)
}

pub(crate) fn deserialize_forward<'de, T, D>(deserializer: D) -> Result<T, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    T::deserialize(deserializer)
}
