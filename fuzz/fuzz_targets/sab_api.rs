#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(json) = serde_json::from_slice(data) {
        let _ = nzb_core::sabnzbd_import::parse_sabnzbd_api_response(&json);
    }
});
