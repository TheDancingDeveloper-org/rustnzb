#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = nzb_core::nzb_parser::parse_nzb("fuzz.nzb", data);
});
