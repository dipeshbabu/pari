use pari_index::{LshError, LshIndex32, LshIndex64, LshParams};

#[test]
fn explicit_configuration_validation_checks_boundaries_without_building_tables() {
    let maximum = usize::try_from(u32::MAX).expect("supported platform");
    assert!(LshParams::new(maximum, 1).validate(1.0, maximum).is_ok());
    assert!(LshParams::new(1, maximum).validate(0.1, maximum).is_ok());
    for threshold in [0.0, -0.1, 1.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            LshParams::new(2, 4).validate(threshold, 8),
            Err(LshError::InvalidThreshold { .. })
        ));
    }
    assert!(matches!(
        LshParams::new(1, 1).validate(0.8, 0),
        Err(LshError::InvalidPermutationCount { .. })
    ));
    for params in [
        LshParams::new(0, 1),
        LshParams::new(1, 0),
        LshParams::new(3, 3),
        LshParams::new(usize::MAX, 2),
    ] {
        let error = params
            .validate(0.8, 8)
            .expect_err("invalid geometry")
            .to_string();
        assert_eq!(
            LshIndex32::with_params(0.8, 8, 7, params)
                .expect_err("affine32 validation")
                .to_string(),
            error
        );
        assert_eq!(
            LshIndex64::with_params(0.8, 8, 7, params)
                .expect_err("affine64 validation")
                .to_string(),
            error
        );
    }
    assert!(LshIndex32::with_params(0.8, 8, 7, LshParams::new(2, 4)).is_ok());
    assert!(LshIndex64::with_params(0.8, 8, 7, LshParams::new(2, 4)).is_ok());
}
