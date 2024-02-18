//! PrimeField implementation of Rings Snark
//! ===============
use crate::prelude::bellman;
use crypto_bigint::rand_core::RngCore;
use crypto_bigint::rand_core;
use serde::de::Deserialize;
use serde::Serialize;
use std::hash::Hash;
use std::hash::Hasher;
use std::marker::PhantomData;

/// We need this struct to make rand-0.4 and rand-0.8 compatible.
/// RngMutRef holding a mut ref of Rng
pub struct RngMutRef<'a, T: rand::Rng> {
    inner: &'a mut T
}

impl <'a, T: rand::Rng> From<&'a mut T> for RngMutRef<'a, T> {
    fn from(inner: &'a mut T) -> Self {
	Self {
	    inner
	}
    }
}

impl <T: rand::Rng> RngCore for RngMutRef<'_, T> {
    fn next_u32(&mut self) -> u32 {
	self.inner.next_u32()
    }
    fn next_u64(&mut self) -> u64 {
	self.inner.next_u64()
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
	self.inner.fill_bytes(dest)
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
	Ok(self.inner.fill_bytes(dest))
    }
}

/// A wrapper structure of [ff::PrimeField]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PrimeField<T: ff::PrimeField> {
    inner: T,
    _phantom: PhantomData<T>

}

/// bellman::PrimeField::Repr Sized + Copy + Clone + Eq + Ord + Send + Sync + Default + Debug + Display + 'static + Rand + AsRef<[u64]> + AsMut<[u64]> + From<u64> + Hash + Serialize + DeserializeOwned
/// ff::PrimeField::Repr Copy + Default + Send + Sync + 'static + AsRef<[u8]> + AsMut<[u8]>
#[derive(Clone, Debug)]
pub struct PrimeFieldRepr<F: ff::PrimeField> {
    _phantom: PhantomData<F::Repr>,
    data: Vec<u64>,
}

impl <F> From<PrimeField<F>> for PrimeFieldRepr<F>
where
    F: ff::PrimeField,
{
    fn from(field: PrimeField<F>) -> Self {
	let repr = field.inner.to_repr();
	let data: &[u8] = repr.as_ref();
	let data = bytes_to_u64_vec_with_padding(data);
	Self {
	    _phantom: PhantomData,
	    data: data
	}
    }
}


impl<F> std::fmt::Display for PrimeFieldRepr<F>
where
    F: ff::PrimeField
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.data)
    }
}

impl<F> From<u64> for PrimeFieldRepr<F>
where
    F: ff::PrimeField
{
    fn from(value: u64) -> Self {
	let value = value as u128;
	let field: PrimeField<F> = F::from_u128(value).into();
	field.into()
    }
}

impl<F> AsRef<[u64]> for PrimeFieldRepr<F>
where
    F: ff::PrimeField
{
    fn as_ref(&self) -> &[u64] {
	&self.data
    }
}

impl<F> AsMut<[u64]> for PrimeFieldRepr<F>
where
    F: ff::PrimeField
{
    fn as_mut(&mut self) -> &mut [u64] {
	&mut self.data
    }
}


impl <T: ff::PrimeField> From<T> for PrimeField<T> {
    fn from(f: T) -> PrimeField<T> {
	Self {
	    inner: f,
	    _phantom: PhantomData
	}
    }
}

impl<T: ff::PrimeField> Hash for PrimeField<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let repr = self.inner.to_repr();
        repr.as_ref().hash(state);
    }
}

impl <T: ff::PrimeField> Serialize for PrimeField<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer {
	let data: Vec<u8> = self.inner.to_repr().as_ref().to_vec();
	data.serialize(serializer)
    }
}

impl<'de, T> Deserialize<'de> for PrimeField<T>
where
    T: ff::PrimeField,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let data: Vec<u8> = Vec::deserialize(deserializer)?;
	let s = std::str::from_utf8(&data).expect("Found invalid UTF-8");
        if let Some(ret) = T::from_str_vartime(s).map(|inner| inner.into())
	{
	    Ok(ret)
	} else {
	    Err(serde::de::Error::custom("Failed to parse str repr"))
	}
    }
}

impl <T: ff::PrimeField> std::fmt::Display for PrimeField<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.inner)
    }
}

impl <T: ff::PrimeField> AsRef<PrimeField<T>> for PrimeField<T> {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl <T: ff::PrimeField> AsRef<T> for PrimeField<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

impl <T: ff::PrimeField> rand::Rand for PrimeField<T> {
    fn rand<R: rand::Rng>(rng: &mut R) -> Self {
	let rng: RngMutRef<R> = rng.into();
	T::random(rng).into()
    }
}

impl <T: ff::PrimeField> bellman::Field for PrimeField<T> {
    fn zero() -> Self {
	T::ZERO.into()
    }
    fn one() -> Self {
	T::ONE.into()
    }
    fn is_zero(&self) -> bool {
	self.inner.is_zero().into()
    }
    fn square(&mut self) {
	self.inner = self.inner.square();
    }
    fn double(&mut self) {
	self.inner = self.inner.double();
    }
    fn negate(&mut self) {
	self.inner = self.inner.neg();
    }
    fn add_assign(&mut self, other: &Self) {
	self.inner.add_assign(other.inner)
    }
    fn sub_assign(&mut self, other: &Self) {
	self.inner.sub_assign(other.inner)

    }
    fn mul_assign(&mut self, other: &Self) {
	self.inner.mul_assign(other.inner)

    }
    fn inverse(&self) -> Option<Self> {
	let ret: Option<T> = self.inner.invert().into();
	ret.map(|r| r.into())
    }

    // todo: just power?
    fn frobenius_map(&mut self, power: usize) {
        if power == 0 {
            *self = Self::one();
            return;
        }

        let mut result = Self::one();
        let mut base = self.clone();
        let mut exp = power;

        while exp > 0 {
            if exp % 2 == 1 {
                result.mul_assign(&base);
            }
            base.square();
            exp /= 2;
        }

        *self = result;
    }

}


// impl <T: ff::PrimeField> bellman::PrimeField for PrimeField<T>
// {
//     type Repr = T::Repr;
//     const NUM_BITS: u32 = T::NUM_BITS;
//     const CAPACITY: u32 = T::CAPACITY;
//     const S: u32 = T::S;

//     fn from_repr(repr: Self::Repr) -> Result<Self, bellman::PrimeFieldDecodingError> {
// 	T::from_repr(repr)
//     }
//     fn from_raw_repr(repr: Self::Repr) -> Result<Self, bellman::PrimeFieldDecodingError> {
// 	T::from_repr(repr)
//     }
//     fn into_repr(&self) -> Self::Repr {
// 	self.inner.to_repr()
//     }
//     fn into_raw_repr(&self) -> Self::Repr {
// 	self.inner.to_repr()
//     }
//     fn char() -> Self::Repr {
// 	T::Repr
//     }
//     fn multiplicative_generator() -> Self {
// 	Self
//     }
//     fn root_of_unity() -> Self {
// 	Self
//     }

// }


pub(crate) fn bytes_to_u64_vec_with_padding(bytes: &[u8]) -> Vec<u64> {
    // Calculate the number of bytes needed to pad the array to a multiple of 8
    let padding = if bytes.len() % 8 == 0 { 0 } else { 8 - (bytes.len() % 8) };

    // Create a new Vec<u8> and extend it with the original bytes plus necessary padding
    let mut padded_bytes = Vec::with_capacity(bytes.len() + padding);
    padded_bytes.extend_from_slice(bytes);

    // Pad with zeros to make the length a multiple of 8
    padded_bytes.resize(bytes.len() + padding, 0);

    // Convert the padded byte slice into a Vec<u64>
    padded_bytes
        .chunks(8)
        .map(|chunk| {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(chunk);
            u64::from_le_bytes(arr) // Or use from_be_bytes, depending on your byte order requirements
        })
        .collect()
}


#[cfg(test)]
pub mod tests {
    use super::*;

    fn test_bytes_to_u64() {
	let bytes = &[1, 2, 3, 4, 5]; // Length is not a multiple of 8
	let u64_vec = bytes_to_u64_vec_with_padding(bytes);
    }
}
