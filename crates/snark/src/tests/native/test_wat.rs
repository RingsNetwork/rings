use crate::error::Error;
use crate::witness::calculator::u256_from_vec_u32;
use crate::witness::calculator::WitnessCalculator;

#[test]
fn test_wat_module_initializes_circom1_witness_calculator_metadata(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let wasm = wat::parse_str(
        r#"
        (module
          (import "env" "memory" (memory 2000))
          (func (export "getFrLen") (result i32)
            i32.const 40)
          (func (export "getPRawPrime") (result i32)
            i32.const 131071968)
          (data (i32.const 131071968)
            "\11\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00"))
        "#,
    )?;
    let store = WitnessCalculator::new_store();
    let module = wasmer::Module::from_binary(&store, &wasm)?;
    let calculator = WitnessCalculator::from_module(module, store)?;

    assert_eq!(calculator.circom_version, 1);
    assert_eq!(calculator.n64, 1);
    Ok(())
}

#[test]
fn test_wat_module_missing_required_export_returns_typed_error(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let wasm = wat::parse_str(
        r#"
        (module
          (import "env" "memory" (memory 2000))
          (func (export "getPRawPrime") (result i32)
            i32.const 131071968))
        "#,
    )?;
    let store = WitnessCalculator::new_store();
    let module = wasmer::Module::from_binary(&store, &wasm)?;
    let error = match WitnessCalculator::from_module(module, store) {
        Ok(_) => {
            return Err("expected missing getFrLen export to fail".into());
        }
        Err(error) => error,
    };

    assert!(
        matches!(&error, Error::WitnessMissingExport(name) if name == "getFrLen"),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn test_wat_module_unsupported_circom_version_returns_typed_error(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let wasm = wat::parse_str(
        r#"
        (module
          (import "env" "memory" (memory 2000))
          (func (export "getVersion") (result i32)
            i32.const 3))
        "#,
    )?;
    let store = WitnessCalculator::new_store();
    let module = wasmer::Module::from_binary(&store, &wasm)?;
    let error = match WitnessCalculator::from_module(module, store) {
        Ok(_) => {
            return Err("expected unsupported Circom version to fail".into());
        }
        Err(error) => error,
    };

    assert!(
        matches!(error, Error::WitnessUnsupportedCircomVersion(3)),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn test_u256_from_vec_u32_rejects_invalid_word_count(
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let error = match u256_from_vec_u32(&[1, 2, 3]) {
        Ok(_) => {
            return Err("expected invalid U256 word count to fail".into());
        }
        Err(error) => error,
    };

    assert!(
        matches!(error, Error::WitnessInvalidU256WordLength {
            expected: 8,
            actual: 3
        }),
        "{error:?}"
    );
    Ok(())
}
