use super::*;

fn value(ty: FourCc, bytes: &[u8]) -> SmcValue {
    SmcValue::new(ty, bytes.to_vec())
}

// The codecs, against bytes read off a real Mac (a Mac16,7, 2026-10-18).

/// `F0Tg`, `F0Mx` and `F0Ac` as read: `flt ` is little-endian.
#[test]
fn float_decodes_the_measured_bytes() {
    assert_eq!(
        value(FourCc::FLT, &hex("00409c45")).float_value(),
        Ok(5000.0)
    );
    assert_eq!(
        value(FourCc::FLT, &hex("0088b445")).float_value(),
        Ok(5777.0)
    );
    let measured = value(FourCc::FLT, &hex("73319d45")).float_value().unwrap();
    assert!((measured - 5030.2).abs() < 0.05, "{measured}");
}

#[test]
fn float_encode_is_the_inverse() {
    assert_eq!(SmcValue::flt(5000.0).unwrap().bytes, hex("00409c45"));
    assert_eq!(SmcValue::flt(5777.0).unwrap().bytes, hex("0088b445"));
    assert_eq!(SmcValue::flt(5030.2).unwrap().float_value(), Ok(5030.2_f32));
    assert_eq!(SmcValue::flt(0.0).unwrap().bytes, [0, 0, 0, 0]);
}

/// A root helper sends what `flt` returns straight to the SMC, so NaN, the
/// infinities and negatives stop here: none of them is a fan speed.
#[test]
fn float_encode_refuses_non_finite_and_negative() {
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -1.0, -0.5] {
        match SmcValue::flt(bad) {
            Err(SmcError::OutOfRange { ty, value }) => {
                assert_eq!(ty, FourCc::FLT);
                assert!(
                    if bad.is_nan() {
                        value.is_nan()
                    } else {
                        value == f64::from(bad)
                    },
                    "{bad}: {value}"
                );
            }
            other => panic!("{bad}: {other:?}"),
        }
    }
}

/// `write` sends nothing unless the value is the key's own type and size:
/// a `ui8` to a `flt ` key would be a fan target of whatever byte it is.
#[test]
fn write_check_refuses_wrong_type_and_size() {
    let fan_target = SmcKeyInfo {
        ty: FourCc::FLT,
        size: 4,
    };
    assert_eq!(
        check_write(&SmcValue::flt(3000.0).unwrap(), fan_target),
        Ok(())
    );
    assert_eq!(
        check_write(&SmcValue::ui8(1), fan_target),
        Err(SmcError::WrongType {
            expected: FourCc::FLT,
            actual: FourCc::UI8
        })
    );
    assert_eq!(
        check_write(&value(FourCc::FLT, &[0, 0]), fan_target),
        Err(SmcError::WrongSize {
            ty: FourCc::FLT,
            expected: 4,
            actual: 2
        })
    );
    // Empty is a size mismatch even against a key that declares 0 bytes.
    assert_eq!(
        check_write(
            &value(FourCc::UI8, &[]),
            SmcKeyInfo {
                ty: FourCc::UI8,
                size: 0
            }
        ),
        Err(SmcError::WrongSize {
            ty: FourCc::UI8,
            expected: 0,
            actual: 0
        })
    );
    // And past the protocol's 32 bytes, whatever the key declares.
    assert_eq!(
        check_write(
            &value(FourCc::UI8, &[0; 33]),
            SmcKeyInfo {
                ty: FourCc::UI8,
                size: 33
            }
        ),
        Err(SmcError::WrongSize {
            ty: FourCc::UI8,
            expected: 33,
            actual: 33
        })
    );
}

/// A refused key-table index call names an index, not a missing `#KEY`.
#[test]
fn key_index_refusal_is_its_own_error() {
    assert_eq!(
        SmcError::check_index(0, 0x84, 7),
        Some(SmcError::KeyIndexRefused {
            index: 7,
            result: 0x84
        })
    );
    assert_eq!(SmcError::check_index(5, 0, 7), Some(SmcError::Kernel(5)));
    assert_eq!(SmcError::check_index(0, 0, 7), None);
}

#[test]
fn integer_types_are_big_endian() {
    assert_eq!(value(FourCc::UI8, &[2]).uint8_value(), Ok(2));
    assert_eq!(value(FourCc::UI16, &[1, 2]).uint16_value(), Ok(0x0102));
    assert_eq!(SmcValue::ui16(0x0102).bytes, [1, 2]);
    assert_eq!(SmcValue::ui8(1).bytes, [1]);
    // `#KEY` on the measured Mac: 3,462 keys.
    assert_eq!(
        value(FourCc::UI32, &[0, 0, 0x0d, 0x86]).uint32_value(),
        Ok(3462)
    );
}

/// `fpe2`, Intel's fan-speed type: 14.2 fixed point, big-endian. 5000 rpm
/// is 20000 quarters, 0x4e20.
#[test]
fn fpe2_round_trips() {
    assert_eq!(value(FourCc::FPE2, &[0x4e, 0x20]).fpe2_value(), Ok(5000.0));
    assert_eq!(SmcValue::fpe2(5000.0).unwrap().bytes, [0x4e, 0x20]);
    assert_eq!(SmcValue::fpe2(1350.25).unwrap().fpe2_value(), Ok(1350.25));
    assert_eq!(
        SmcValue::fpe2(16_384.0),
        Err(SmcError::OutOfRange {
            ty: FourCc::FPE2,
            value: 16_384.0
        })
    );
    assert!(SmcValue::fpe2(-1.0).is_err());
    assert!(SmcValue::fpe2(f64::NAN).is_err());
}

/// A fan speed is `flt ` on Apple Silicon and `fpe2` on Intel; the number
/// is the same either way.
#[test]
fn numeric_value_reads_either_fan_type() {
    assert_eq!(
        value(FourCc::FLT, &hex("00409c45")).numeric_value(),
        Ok(5000.0)
    );
    assert_eq!(
        value(FourCc::FPE2, &[0x4e, 0x20]).numeric_value(),
        Ok(5000.0)
    );
    assert_eq!(
        value(code("sp78"), &[0x40, 0]).numeric_value(),
        Err(SmcError::UnsupportedType(code("sp78")))
    );
}

/// A decode of the wrong type or size is an error, never a number read
/// from bytes that mean something else.
#[test]
fn wrong_type_and_size_are_errors() {
    assert_eq!(
        value(FourCc::UI8, &[1]).float_value(),
        Err(SmcError::WrongType {
            expected: FourCc::FLT,
            actual: FourCc::UI8
        })
    );
    assert_eq!(
        value(FourCc::FLT, &[0, 0]).float_value(),
        Err(SmcError::WrongSize {
            ty: FourCc::FLT,
            expected: 4,
            actual: 2
        })
    );
}

#[test]
fn four_cc_round_trips_and_rejects_wrong_lengths() {
    assert_eq!(code("TCMz").to_string(), "TCMz");
    assert_eq!(FourCc::parse("flt ").map(|c| c.0), Some(0x666c_7420));
    assert_eq!(FourCc::parse("flt"), None);
    assert_eq!(FourCc::parse("F10Ac"), None);
    assert_eq!(fan_key(fan_index(1), *b"md"), code("F1md"));
    assert_eq!(FourCc(0x0000_4142).to_string(), "??AB");
}

/// 0xe00002c1, the non-root write on 2026-10-18, is its own case and never
/// a generic kernel error: a root helper exists because of this one.
#[test]
fn not_privileged_is_its_own_case() {
    let key = code("F0Tg");
    assert_eq!(
        SmcError::check(0xe000_02c1_u32 as i32, 0, key),
        Some(SmcError::NotPrivileged)
    );
    assert_eq!(
        SmcError::check(0, 0x84, code("TCMz")),
        Some(SmcError::KeyNotFound(code("TCMz")))
    );
    assert_eq!(
        SmcError::check(0, 0x85, key),
        Some(SmcError::SmcRefused { key, result: 0x85 })
    );
    assert_eq!(SmcError::check(5, 0, key), Some(SmcError::Kernel(5)));
    assert_eq!(SmcError::check(0, 0, key), None);
}
