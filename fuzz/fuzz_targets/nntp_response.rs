#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(line) = std::str::from_utf8(data) {
        let _ = nzb_nntp::connection::parse_response_line(line);
    }
    let _ = nzb_nntp::connection::parse_xover_data(data);
    let _ = nzb_nntp::connection::parse_header_data(data);
    let _ = nzb_nntp::connection::parse_list_active_data(data);
});
