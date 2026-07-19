use crate::error::Error;
use crate::error::Result;
use crate::prelude::nova::provider::VestaEngine;
use crate::prelude::nova::traits::Engine;
use crate::r1cs;

const SIMPLE_BN256_WASM: &str = "src/tests/native/circoms/simple_bn256.wasm";

fn set_test_input<F: Copy>(values: &mut [F], index: usize, value: F) -> Result<()> {
    let Some(slot) = values.get_mut(index) else {
        return Err(Error::InvalidDataWhenReadingR1CS(format!(
            "missing test input index {index}"
        )));
    };
    *slot = value;
    Ok(())
}

fn field_at<F: Copy>(values: &[F], index: usize) -> Result<F> {
    values
        .get(index)
        .copied()
        .ok_or_else(|| Error::InvalidDataWhenReadingR1CS(format!("missing witness index {index}")))
}

fn field_range<F>(values: &[F], start: usize, end: usize) -> Result<&[F]> {
    values.get(start..end).ok_or_else(|| {
        Error::InvalidDataWhenReadingR1CS(format!("missing witness range {start}..{end}"))
    })
}

#[tokio::test]
pub async fn test_calcu_witness_sha256() -> Result<()> {
    type F = <VestaEngine as Engine>::Scalar;
    let r1cs = r1cs::load_r1cs_local::<F>(
        "src/tests/native/circoms/test_sha256.r1cs",
        r1cs::Format::Bin,
    )?;
    assert_eq!(r1cs.num_inputs, 513, "wrong inputs {:?}", r1cs.num_inputs);
    // 1 + 256 + 256
    let mut witness_calculator = r1cs::load_circom_witness_calculator(r1cs::Path::Local(
        "src/tests/native/circoms/test_sha256.wasm".to_string(),
    ))
    .await?;
    let mut input_inner = [F::from(0); 256].to_vec();
    set_test_input(&mut input_inner, 0, F::from(0u64))?;
    set_test_input(&mut input_inner, 1, F::from(1u64))?;
    let input = vec![("in".to_string(), input_inner.clone())];

    let witness = witness_calculator.calculate_witness::<F>(input, true)?;
    assert_eq!(field_at(&witness, 0)?, F::from(1u64));
    // 1 output:256 input: 256
    // 0 1-257 258-513
    assert_eq!(
        field_at(&witness, 258)?,
        F::from(1u64),
        "input is not included, 257-267: {:?}",
        field_range(&witness, 257, 267)?
    );
    assert_eq!(
        field_at(&witness, 259)?,
        F::from(0u64),
        "input is not included, 257-267: {:?}",
        field_range(&witness, 257, 267)?
    );
    Ok(())
}

#[tokio::test]
pub async fn test_calcu_witness_bn256() -> Result<()> {
    type F = <VestaEngine as Engine>::Base;
    let mut witness_calculator =
        r1cs::load_circom_witness_calculator(r1cs::Path::Local(SIMPLE_BN256_WASM.to_string()))
            .await?;
    let input = vec![("step_in".to_string(), vec![F::from(4u64), F::from(2u64)])];
    let witness = witness_calculator.calculate_witness::<F>(input, true)?;
    assert_eq!(field_at(&witness, 0)?, F::from(1u64));
    // witness: <1> <Outputs> <Inputs> <Auxs>
    // test input
    assert_eq!(
        field_at(&witness, 3)?,
        F::from(4u64),
        "input is not included, {:?}",
        witness
    );
    assert_eq!(
        field_at(&witness, 4)?,
        F::from(2u64),
        "input is not included, {:?}",
        witness
    );
    Ok(())
}

#[tokio::test]
pub async fn test_load_witness_local_fixture() -> Result<()> {
    type F = <VestaEngine as Engine>::Base;

    let mut witness_calculator =
        r1cs::load_circom_witness_calculator(r1cs::Path::Local(SIMPLE_BN256_WASM.to_string()))
            .await?;
    let input = vec![("step_in".to_string(), vec![F::from(4u64), F::from(2u64)])];
    let witness = witness_calculator.calculate_witness::<F>(input, true)?;
    assert_eq!(field_at(&witness, 0)?, F::from(1u64));
    Ok(())
}
