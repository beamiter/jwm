//! Capture-thread-only one-shot text delivery under the original frozen context.
use super::*;
pub(super) fn process(
    sources: &SourceControl,
    capture: &mut HostCapture,
    writer: &SharedWriter,
) -> RemoteResult<()> {
    let request = sources.text_request.lock().unwrap().take();
    let Some(request) = request else {
        return Ok(());
    };
    let tracked = *sources.pointer_frame.lock().unwrap();
    let drawn = sources.state.lock().unwrap().presented_frame;
    let valid = tracked.filter(|frame| {
        frame.scope == Some(request.scope)
            && drawn == Some(request.scope)
            && frame.at.elapsed() < Duration::from_secs(1)
    });
    let accepted = valid.is_some_and(|frame| {
        capture
            .text_frame()
            .is_some_and(|(serial, _)| serial == frame.serial)
            && capture.text_commit(&request.text, frame.serial).is_ok()
    });
    capture.finish_text();
    // Both outcomes consume this wire attempt; no second context/lease is created.
    sources.text_enabled.store(false, Ordering::Release);
    sources.pointer_frame.lock().unwrap().take();
    let mut writer = lock_writer(writer);
    writer.write_message(
        MessageKind::WaylandTextResult,
        &super::super::wayland_text_wire::TextResult {
            scope: request.scope,
            accepted,
        }
        .encode()?,
    )?;
    writer.flush()?;
    Ok(())
}
