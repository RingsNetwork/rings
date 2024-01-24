//! Utils for r1cs
pub mod reader;

use std::io::Cursor;

use ff::PrimeField;
use serde::Deserialize;
use serde::Serialize;
use wasmer::Module;

use crate::error::Error;
use crate::error::Result;
use crate::witness::calculator::WitnessCalculator;

/// type of constraint
pub(crate) type Constraint<F> = (Vec<(usize, F)>, Vec<(usize, F)>, Vec<(usize, F)>);

/// R1CS
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct R1CS<F: PrimeField> {
    /// number inputs of r1cs
    pub num_inputs: usize,
    /// number aux variable of r1cs
    pub num_aux: usize,
    /// number of total variables
    pub num_variables: usize,
    /// constraints
    pub constraints: Vec<Constraint<F>>,
}

/// Path of a r1cs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// Local filesystem path
    Local(String),
    /// A remote resource needs to fetch
    Remote(String),
}

/// format of R1CS, can be bin or json format
pub enum Format {
    /// JSON format
    Json,
    /// BIN Format
    Bin,
}

/// Define witness, which is a vector of prime field.
pub type TyWitness<F> = Vec<F>;

/// Fetch remote resources to Read
pub(crate) async fn fetch(url: &str) -> Result<Cursor<Vec<u8>>> {
    let resp = reqwest::get(url).await?;
    let bytes = resp.bytes().await?;
    Ok(Cursor::new(bytes.to_vec()))
}

/// Fetch remote r1cs
pub async fn load_r1cs_remote<F: PrimeField>(url: &str, format: Format) -> Result<R1CS<F>> {
    let data = fetch(url).await?;
    let ret = match format {
        Format::Json => reader::load_r1cs_from_json::<F, Cursor<Vec<u8>>>(data),
        Format::Bin => reader::load_r1cs_from_bin::<F, Cursor<Vec<u8>>>(data),
    };
    Ok(ret.into())
}

/// Load local r1cs
pub fn load_r1cs_local<F: PrimeField>(
    path: impl AsRef<std::path::Path>,
    format: Format,
) -> Result<R1CS<F>> {
    let ret = match format {
        Format::Json => reader::load_r1cs_from_json_file::<F>(path),
        Format::Bin => reader::load_r1cs_from_bin_file::<F>(path),
    };
    Ok(ret.into())
}

/// Load r1cs, the resource path can be remote local, and both bin and json are supported
pub async fn load_r1cs<F: PrimeField>(path: Path, format: Format) -> Result<R1CS<F>> {
    let ret = match path {
        Path::Local(p) => load_r1cs_local::<F>(p, format)?,
        Path::Remote(url) => load_r1cs_remote::<F>(&url, format).await?,
    };
    Ok(ret)
}

/// Fetch remote witness
pub async fn load_witness_remote<F: PrimeField>(url: &str, format: Format) -> Result<TyWitness<F>> {
    let data = fetch(url).await?;
    let ret = match format {
        Format::Json => reader::load_witness_from_bin_reader::<F, Cursor<Vec<u8>>>(data)
            .map_err(|e| Error::WitnessFailedOnLoad(e.to_string()))?,
        Format::Bin => reader::load_witness_from_json::<F, Cursor<Vec<u8>>>(data),
    };
    Ok(ret)
}

/// Load witness local
pub fn load_witness_local<F: PrimeField>(
    path: impl AsRef<std::path::Path>,
    format: Format,
) -> Result<TyWitness<F>> {
    let ret = match format {
        Format::Json => reader::load_witness_from_bin_file::<F>(path),
        Format::Bin => reader::load_witness_from_json_file::<F>(path),
    };
    Ok(ret)
}

/// Load r1cs, the resource path can be remote local, and both bin and json are supported
pub async fn load_witness<F: PrimeField>(path: Path, format: Format) -> Result<TyWitness<F>> {
    match path {
        Path::Local(p) => load_witness_local::<F>(p, format),
        Path::Remote(url) => load_witness_remote::<F>(&url, format).await,
    }
}

/// Load witness calculator from local path
pub fn load_circom_witness_calculator_local(
    path: impl AsRef<std::path::Path>,
) -> Result<WitnessCalculator> {
    WitnessCalculator::from_file(path).map_err(|e| Error::WASMFailedToLoad(e.to_string()))
}

/// Load witness calculator from remote path
pub async fn load_circom_witness_calculator_remote(path: &str) -> Result<WitnessCalculator> {
    let store = WitnessCalculator::new_store();
    let data = fetch(path).await?;
    let module = Module::from_binary(&store, data.get_ref().as_slice())
        .map_err(Error::WitnessCompileError)?;
    Ok(WitnessCalculator::from_module(module, store)?)
}

/// Load witness calculator from local path or remote url
pub async fn load_circom_witness_calculator(path: Path) -> Result<WitnessCalculator> {
    match path {
        Path::Local(p) => load_circom_witness_calculator_local(p),
        Path::Remote(url) => load_circom_witness_calculator_remote(&url).await,
    }
}
