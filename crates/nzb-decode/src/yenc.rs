//! yEnc decoder — delegates to the `yenc-simd` crate (SIMD-accelerated).

pub use yenc_simd::{YencDecodeResult, YencError, decode_yenc, encode_article};

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = b"Hello, this is test data for yEnc roundtrip!";
        let (encoded, _crc) = encode_article(original, "test.bin", 1, 1, 0, original.len() as u64);
        let result = decode_yenc(&encoded).unwrap();
        assert_eq!(result.data, original);
        assert_eq!(result.filename.as_deref(), Some("test.bin"));
    }

    #[test]
    fn test_decode_empty_input() {
        let result = decode_yenc(b"");
        assert!(result.is_err());
    }

    #[test]
    fn test_decode_malformed_input() {
        let result = decode_yenc(b"not a valid yenc payload at all");
        assert!(result.is_err());
    }

    #[test]
    fn test_encode_decode_binary_data() {
        let original: Vec<u8> = (0..=255).collect();
        let (encoded, _crc) =
            encode_article(&original, "binary.bin", 1, 1, 0, original.len() as u64);
        let result = decode_yenc(&encoded).unwrap();
        assert_eq!(result.data, original);
    }

    #[test]
    fn test_decode_crc32_verification() {
        let original = b"CRC32 test payload";
        let (encoded, expected_crc) =
            encode_article(original, "crc.bin", 1, 1, 0, original.len() as u64);
        let result = decode_yenc(&encoded).unwrap();
        assert_eq!(result.crc32, expected_crc);
    }

    #[test]
    fn test_encode_decode_large_payload() {
        let original: Vec<u8> = (0..10_000).map(|i| (i % 256) as u8).collect();
        let (encoded, _crc) =
            encode_article(&original, "large.bin", 1, 1, 0, original.len() as u64);
        let result = decode_yenc(&encoded).unwrap();
        assert_eq!(result.data, original);
    }

    proptest! {
        #[test]
        fn encode_decode_roundtrips_arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 1..4096)) {
            let (encoded, _) = encode_article(&data, "property.bin", 1, 1, 0, data.len() as u64);
            let decoded = decode_yenc(&encoded).expect("encoder must produce decodable data");
            prop_assert_eq!(decoded.data, data);
        }
    }
}
