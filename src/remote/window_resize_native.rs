//! Fixed local IPC adapter. No network peer can choose a command, XID or nonce.
use super::RemoteResult;
use serde_json::{Value, json};
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

pub(super) struct NativeResizeGrant {
    reader: BufReader<UnixStream>,
    grant: u64,
}

impl NativeResizeGrant {
    pub(super) fn connect(window: u32, challenge: [u8; 32]) -> RemoteResult<Self> {
        let path = crate::ipc_server::validated_socket_path()?;
        let stream = crate::ipc_connection::connect(&path, Duration::from_secs(1))?;
        let mut this = Self {
            reader: BufReader::new(stream),
            grant: 0,
        };
        let data = this.exchange(
            "remote_resize_grant",
            json!({"window":window,"challenge":challenge}),
        )?;
        this.grant = data["grant"]
            .as_u64()
            .filter(|v| *v != 0)
            .ok_or_else(|| io::Error::other("invalid native resize grant"))?;
        dimensions(&data)?;
        Ok(this)
    }

    fn exchange(&mut self, command: &str, args: Value) -> RemoteResult<Value> {
        let mut request = serde_json::to_vec(&json!({"command":command,"args":args}))?;
        request.push(b'\n');
        if request.len() > 1024 {
            return Err(io::Error::other("native resize request too large").into());
        }
        crate::ipc_connection::write_all_with_timeout(
            self.reader.get_mut(),
            &request,
            Duration::from_secs(1),
        )?;
        let response = jwm_ipc_transport::read_frame_until(
            &mut self.reader,
            4096,
            Instant::now() + Duration::from_secs(1),
        )?;
        let response: Value = serde_json::from_slice(&response)?;
        if response["success"] != true {
            return Err(io::Error::other("native resize refused").into());
        }
        Ok(response["data"].clone())
    }

    pub(super) fn apply(
        &mut self,
        expected: (u16, u16),
        target: (u16, u16),
    ) -> RemoteResult<(u16, u16)> {
        let response = self.exchange("remote_resize_apply", json!({"grant":self.grant,
            "expected_width":expected.0,"expected_height":expected.1,"width":target.0,"height":target.1}))?;
        if !matches!(
            response["status"].as_str(),
            Some("configured" | "unchanged")
        ) {
            return Err(io::Error::other("invalid native resize acknowledgement").into());
        }
        dimensions(&response)
    }
}
// Closing the sole socket revokes its connection-scoped grant, including errors.
fn dimensions(data: &Value) -> RemoteResult<(u16, u16)> {
    let parse = |field: &str| {
        data[field]
            .as_u64()
            .and_then(|v| u16::try_from(v).ok())
            .ok_or_else(|| io::Error::other("invalid native resize dimensions"))
    };
    let size = (parse("width")?, parse("height")?);
    super::frame::validate_dimensions(size.0, size.1)?;
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_resize_dimensions_reject_overflow_and_budget() {
        for value in [
            json!({"width":0,"height":1}),
            json!({"width":65537,"height":1}),
            json!({"width":16384,"height":16384}),
            json!({"width":-1,"height":1}),
        ] {
            assert!(dimensions(&value).is_err());
        }
        assert_eq!(
            dimensions(&json!({"width":640,"height":480})).unwrap(),
            (640, 480)
        );
    }
}
