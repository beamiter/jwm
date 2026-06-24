use std::sync::Arc;
use x11rb::protocol::randr::ConnectionExt as RandrExt;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::rust_connection::RustConnection;

pub use crate::backend::edid::{parse_edid_hdr_from_bytes, EdidHdrCapabilities};

pub fn query_edid_hdr(conn: &Arc<RustConnection>, output: u32) -> Option<EdidHdrCapabilities> {
    let edid_atom = conn.intern_atom(false, b"EDID").ok()?.reply().ok()?.atom;

    let prop = conn
        .randr_get_output_property(output, edid_atom, 0u32, 0, 256, false, false)
        .ok()?
        .reply()
        .ok()?;

    if prop.data.len() < 128 {
        return None;
    }

    parse_edid_hdr_from_bytes(&prop.data)
}
