//! Coalesce recording requests until the compositor can apply them with GL current.
//!
//! A stop cancels earlier, unconsumed starts. A later start keeps the stop
//! barrier so the old pipeline is retired before the new one is initialized.

use crate::backend::recording_admission::{RecordingAudioOptions, RecordingStartReporter};
use crate::backend::recording_completion::RecordingStopReporter;

type Region = (i32, i32, u32, u32);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RecordingCommand {
    Stop,
    Start {
        path: String,
        region: Region,
        audio: RecordingAudioOptions,
    },
}

type PendingStart = (
    String,
    Region,
    RecordingAudioOptions,
    Option<RecordingStartReporter>,
);
pub(super) struct ReadyCommand {
    pub(super) command: RecordingCommand,
    pub(super) start_reporter: Option<RecordingStartReporter>,
    pub(super) stop_reporter: Option<RecordingStopReporter>,
    pub(super) cancelled_tracked_start: bool,
}

#[derive(Default)]
pub(super) struct PendingRecordingCommands {
    stop: bool,
    stop_reporter: Option<RecordingStopReporter>,
    cancelled_tracked_start: bool,
    start: Option<PendingStart>,
}

impl PendingRecordingCommands {
    pub(super) fn start(&mut self, path: String, region: Region) {
        self.start_tracked(path, region, None);
    }

    pub(super) fn start_tracked(
        &mut self,
        path: String,
        region: Region,
        reporter: Option<RecordingStartReporter>,
    ) {
        self.start_with_audio_tracked(path, region, RecordingAudioOptions::snapshot(), reporter);
    }

    pub(super) fn start_with_audio_tracked(
        &mut self,
        path: String,
        region: Region,
        audio: RecordingAudioOptions,
        reporter: Option<RecordingStartReporter>,
    ) {
        self.start = Some((path, region, audio, reporter));
    }

    pub(super) fn stop(&mut self) {
        self.start = None;
        self.stop = true;
    }

    pub(super) fn stop_tracked(&mut self, reporter: RecordingStopReporter) {
        // This proof belongs only to the tracked Start removed by this
        // exact Stop. A legacy stop or an earlier cancellation cannot lend it.
        let cancelled_tracked_start = self
            .start
            .take()
            .is_some_and(|(_, _, _, reporter)| reporter.is_some());
        self.stop = true;
        if self.stop_reporter.is_some() {
            // The first Stop still owns the old sink. A later Start cancelled
            // before this render owns no child of its own, independently of
            // that older writer (which can be video-only).
            if cancelled_tracked_start {
                reporter.complete_without_encoder();
            } else {
                reporter.complete(None);
            }
        } else {
            self.cancelled_tracked_start = cancelled_tracked_start;
            self.stop_reporter = Some(reporter);
        }
    }

    pub(super) fn set_region(&mut self, region: Region) {
        if let Some((_, pending_region, _, _)) = self.start.as_mut() {
            *pending_region = region;
        }
    }

    pub(super) fn has_start(&self) -> bool {
        self.start.is_some()
    }

    pub(super) fn has_pending(&self) -> bool {
        self.stop || self.has_start()
    }

    /// At most two commands, always retiring an old pipeline before starting one.
    pub(super) fn take(&mut self) -> [Option<ReadyCommand>; 2] {
        let stop = std::mem::take(&mut self.stop).then(|| ReadyCommand {
            command: RecordingCommand::Stop,
            start_reporter: None,
            stop_reporter: self.stop_reporter.take(),
            cancelled_tracked_start: std::mem::take(&mut self.cancelled_tracked_start),
        });
        let start = self
            .start
            .take()
            .map(|(path, region, audio, reporter)| ReadyCommand {
                command: RecordingCommand::Start {
                    path,
                    region,
                    audio,
                },
                start_reporter: reporter,
                stop_reporter: None,
                cancelled_tracked_start: false,
            });
        [stop, start]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGION: Region = (10, 20, 640, 480);

    fn start(path: &str, region: Region) -> RecordingCommand {
        RecordingCommand::Start {
            path: path.into(),
            region,
            audio: RecordingAudioOptions::snapshot(),
        }
    }

    fn drain(pending: &mut PendingRecordingCommands) -> Vec<RecordingCommand> {
        pending
            .take()
            .into_iter()
            .flatten()
            .map(|ready| {
                assert!(ready.start_reporter.is_none());
                assert!(ready.stop_reporter.is_none());
                ready.command
            })
            .collect()
    }

    fn audio(enabled: bool, device: &str, bitrate: &str) -> RecordingAudioOptions {
        RecordingAudioOptions {
            enabled,
            device: device.into(),
            bitrate: bitrate.into(),
        }
    }

    #[test]
    fn cancelled_audio_start_after_video_stop_has_its_own_release_proof() {
        use crate::backend::recording_admission::recording_start_channel;
        use crate::backend::recording_completion::{
            AudioRelease, EncoderWaitOutcome, RecordingStopObservations, recording_stop_channel,
            writer_completion_channel,
        };
        let mut queue = PendingRecordingCommands::default();
        let (old_stop, old_receipt) = recording_stop_channel();
        queue.stop_tracked(old_stop);
        let (start, _start_receipt) = recording_start_channel();
        queue.start_with_audio_tracked(
            "new-audio.mp4".into(),
            REGION,
            audio(true, "default", "96k"),
            Some(start),
        );
        let (new_stop, new_receipt) = recording_stop_channel();
        queue.stop_tracked(new_stop);
        let mut audio_owner = RecordingStopObservations::default();
        audio_owner.observe_with_audio(Some(new_receipt), true);
        assert_eq!(audio_owner.audio_release(), AudioRelease::Available);
        let (old_writer, completion) = writer_completion_channel();
        let ready = queue.take()[0].take().unwrap();
        assert!(!ready.cancelled_tracked_start);
        ready.stop_reporter.unwrap().complete(Some(completion));
        let mut old_owner = RecordingStopObservations::default();
        old_owner.observe_with_audio(Some(old_receipt), false);
        assert_eq!(old_owner.audio_release(), AudioRelease::Available);
        old_writer.finish(None, Some(EncoderWaitOutcome::Exited(true)));
        assert_eq!(audio_owner.audio_release(), AudioRelease::Available);
    }

    #[test]
    fn cancellation_proof_is_bound_to_tracked_stop_and_not_borrowed_by_legacy_stop() {
        use crate::backend::recording_admission::recording_start_channel;
        use crate::backend::recording_completion::{
            AudioRelease, RecordingStopObservations, recording_stop_channel,
        };
        for legacy_start in [false, true] {
            let mut queue = PendingRecordingCommands::default();
            let (start, _receipt) = recording_start_channel();
            queue.start_tracked(
                "pending.mp4".into(),
                REGION,
                (!legacy_start).then_some(start),
            );
            if !legacy_start {
                queue.stop();
            }
            let (stop, receipt) = recording_stop_channel();
            queue.stop_tracked(stop);
            let ready = queue.take()[0].take().unwrap();
            assert!(!ready.cancelled_tracked_start);
            ready.stop_reporter.unwrap().complete(None);
            let mut owner = RecordingStopObservations::default();
            owner.observe_with_audio(Some(receipt), true);
            assert_eq!(owner.audio_release(), AudioRelease::Unconfirmed);
        }
    }

    #[test]
    fn cancellation_proof_moves_once_and_live_writer_still_takes_precedence() {
        use crate::backend::recording_admission::recording_start_channel;
        use crate::backend::recording_completion::{
            AudioRelease, EncoderWaitOutcome, RecordingStopObservations, recording_stop_channel,
            writer_completion_channel,
        };
        let mut queue = PendingRecordingCommands::default();
        let (start, _receipt) = recording_start_channel();
        queue.start_tracked("pending.mp4".into(), REGION, Some(start));
        let (stop, receipt) = recording_stop_channel();
        queue.stop_tracked(stop);
        // An unrelated later legacy start/stop cannot replace the first proof.
        queue.start("legacy.mp4".into(), REGION);
        queue.stop();
        let ready = queue.take()[0].take().unwrap();
        assert!(ready.cancelled_tracked_start);
        let (writer, completion) = writer_completion_channel();
        ready.stop_reporter.unwrap().complete(Some(completion));
        let mut owner = RecordingStopObservations::default();
        owner.observe_with_audio(Some(receipt), true);
        assert_eq!(owner.audio_release(), AudioRelease::Pending);
        writer.finish(None, Some(EncoderWaitOutcome::Exited(true)));
        assert_eq!(owner.audio_release(), AudioRelease::Available);
        let (stop, _receipt) = recording_stop_channel();
        queue.stop_tracked(stop);
        assert!(!queue.take()[0].take().unwrap().cancelled_tracked_start);
    }

    #[test]
    fn queued_audio_snapshot_survives_config_changes_and_crop_updates() {
        let mut pending = PendingRecordingCommands::default();
        for enabled in [false, true] {
            let mut request = audio(enabled, "request-device", "96k");
            let expected = request.clone();
            pending.start_with_audio_tracked("private.mp4".into(), REGION, request.clone(), None);
            request.enabled = !enabled;
            request.device = "later-device".into();
            request.bitrate = "192k".into();
            let region = (20, 30, 320, 240);
            pending.set_region(region);
            assert_eq!(
                drain(&mut pending),
                vec![RecordingCommand::Start {
                    path: "private.mp4".into(),
                    region,
                    audio: expected
                }]
            );
        }
    }

    #[test]
    fn replacement_and_stop_keep_audio_snapshot_with_its_exact_request() {
        use crate::backend::recording_admission::{RecordingStartOutcome, recording_start_channel};
        for stop in [false, true] {
            let mut pending = PendingRecordingCommands::default();
            let (old_reporter, mut old_receipt) = recording_start_channel();
            let (new_reporter, mut new_receipt) = recording_start_channel();
            let old = audio(true, "old", "64k");
            let new = audio(false, "new", "128k");
            pending.start_with_audio_tracked("old.mp4".into(), REGION, old, Some(old_reporter));
            if stop {
                pending.stop();
            }
            pending.start_with_audio_tracked(
                "new.mp4".into(),
                REGION,
                new.clone(),
                Some(new_reporter),
            );
            assert_eq!(old_receipt.poll(), Some(RecordingStartOutcome::Unavailable));
            let commands: Vec<_> = pending.take().into_iter().flatten().collect();
            assert_eq!(commands.len(), if stop { 2 } else { 1 });
            for ready in commands {
                match ready.command {
                    RecordingCommand::Stop => assert!(ready.start_reporter.is_none()),
                    command @ RecordingCommand::Start { .. } => {
                        assert_eq!(
                            command,
                            RecordingCommand::Start {
                                path: "new.mp4".into(),
                                region: REGION,
                                audio: new.clone()
                            }
                        );
                        ready.start_reporter.unwrap().complete(true);
                    }
                }
            }
            assert_eq!(new_receipt.poll(), Some(RecordingStartOutcome::Accepted));
            assert!(!pending.has_pending());
        }
    }

    #[test]
    fn stop_cancels_unconsumed_starts_without_losing_the_stop_barrier() {
        let mut pending = PendingRecordingCommands::default();
        pending.start("discarded.mp4".into(), REGION);
        pending.stop();
        assert!(!pending.has_start());
        assert!(pending.has_pending());
        assert_eq!(drain(&mut pending), vec![RecordingCommand::Stop]);
        assert!(!pending.has_pending());
        assert!(drain(&mut pending).is_empty());
    }

    #[test]
    fn a_new_start_follows_the_stop_barrier_and_keeps_its_latest_region() {
        let mut pending = PendingRecordingCommands::default();
        pending.start("discarded.mp4".into(), REGION);
        pending.stop();
        pending.start("new.mp4".into(), REGION);
        let region = (-10, 30, 320, 240);
        pending.set_region(region);
        assert!(pending.has_start());
        assert_eq!(
            drain(&mut pending),
            vec![RecordingCommand::Stop, start("new.mp4", region)]
        );
        assert!(!pending.has_pending());
    }

    #[test]
    fn repeated_requests_are_bounded_and_the_last_stop_cancels_every_start() {
        let mut pending = PendingRecordingCommands::default();
        assert!(!pending.has_pending());
        pending.set_region(REGION);
        assert!(drain(&mut pending).is_empty());
        pending.start("first.mp4".into(), REGION);
        pending.start("latest.mp4".into(), REGION);
        assert_eq!(drain(&mut pending), vec![start("latest.mp4", REGION)]);
        for _ in 0..16 {
            pending.stop();
            pending.start("discarded.mp4".into(), REGION);
        }
        pending.stop();
        pending.stop();
        assert_eq!(drain(&mut pending), vec![RecordingCommand::Stop]);
        assert!(drain(&mut pending).is_empty());
    }

    #[test]
    fn a_frame_captures_the_old_pipeline_before_applying_the_restart() {
        let mut pending = PendingRecordingCommands::default();
        pending.stop();
        pending.start("new.mp4".into(), REGION);
        let mut active = Some("old.mp4".to_string());
        let mut events = vec![format!("capture:{}", active.as_ref().unwrap())];
        for ready in pending.take().into_iter().flatten() {
            assert!(ready.start_reporter.is_none());
            assert!(ready.stop_reporter.is_none());
            match ready.command {
                RecordingCommand::Stop => {
                    events.push(format!("stop:{}", active.take().unwrap()));
                }
                RecordingCommand::Start { path, .. } => {
                    assert!(active.is_none(), "the old pipeline must be retired first");
                    events.push(format!("start:{path}"));
                    active = Some(path);
                }
            }
        }
        assert_eq!(events, ["capture:old.mp4", "stop:old.mp4", "start:new.mp4"]);
        assert_eq!(active.as_deref(), Some("new.mp4"));
        assert!(drain(&mut pending).is_empty());
    }

    #[test]
    fn every_short_request_sequence_matches_the_last_stop_barrier() {
        for initially_active in [false, true] {
            for len in 0..=6 {
                for bits in 0..(1 << len) {
                    let mut pending = PendingRecordingCommands::default();
                    let mut last_start = None;
                    let mut saw_stop = false;
                    for index in 0..len {
                        if bits & (1 << index) == 0 {
                            let path = format!("session-{index}.mp4");
                            pending.start(path.clone(), REGION);
                            last_start = Some(path);
                        } else {
                            pending.stop();
                            saw_stop = true;
                            last_start = None;
                        }
                        assert!(pending.has_pending());
                        assert_eq!(pending.has_start(), last_start.is_some());
                    }
                    let mut active = initially_active.then(|| "old.mp4".to_string());
                    let commands = drain(&mut pending);
                    assert!(commands.len() <= 2);
                    assert_eq!(commands.first() == Some(&RecordingCommand::Stop), saw_stop);
                    for command in commands {
                        match command {
                            RecordingCommand::Stop => active = None,
                            RecordingCommand::Start { path, .. } => {
                                // Mirrors the real start method's already-active guard.
                                if active.is_none() {
                                    active = Some(path);
                                }
                            }
                        }
                    }
                    let expected = if initially_active && !saw_stop {
                        Some("old.mp4".to_string())
                    } else {
                        last_start
                    };
                    assert_eq!(
                        active, expected,
                        "active={initially_active} bits={bits} len={len}"
                    );
                    assert!(!pending.has_pending());
                    assert!(drain(&mut pending).is_empty());
                }
            }
        }
    }

    #[test]
    fn canceled_or_replaced_pending_starts_drop_only_their_reporter() {
        use crate::backend::recording_admission::{RecordingStartOutcome, recording_start_channel};
        for stop in [false, true] {
            let mut pending = PendingRecordingCommands::default();
            let (old, mut old_receipt) = recording_start_channel();
            pending.start_tracked("old.mp4".into(), REGION, Some(old));
            if stop {
                pending.stop();
            }
            let (new, mut new_receipt) = recording_start_channel();
            pending.start_tracked("new.mp4".into(), REGION, Some(new));
            assert_eq!(old_receipt.poll(), Some(RecordingStartOutcome::Unavailable));
            assert_eq!(new_receipt.poll(), None);
            pending.set_region((20, 30, 800, 600));
            let commands: Vec<_> = pending.take().into_iter().flatten().collect();
            assert_eq!(commands.len(), if stop { 2 } else { 1 });
            for ready in commands {
                assert!(ready.stop_reporter.is_none());
                let reporter = ready.start_reporter;
                match ready.command {
                    RecordingCommand::Stop => assert!(reporter.is_none()),
                    RecordingCommand::Start {
                        path,
                        region,
                        audio,
                    } => {
                        assert_eq!(audio, RecordingAudioOptions::snapshot());
                        assert_eq!(path, "new.mp4");
                        assert_eq!(region, (20, 30, 800, 600));
                        reporter.unwrap().complete(true);
                    }
                }
            }
            assert_eq!(new_receipt.poll(), Some(RecordingStartOutcome::Accepted));
            assert!(pending.take().iter().all(Option::is_none));
        }
    }

    #[test]
    fn compositor_queue_destruction_leaves_admission_unknown() {
        use crate::backend::recording_admission::{RecordingStartOutcome, recording_start_channel};
        let (reporter, mut receipt) = recording_start_channel();
        let mut pending = PendingRecordingCommands::default();
        pending.start_tracked("not-opened.mp4".into(), REGION, Some(reporter));
        drop(pending);
        assert_eq!(receipt.poll(), Some(RecordingStartOutcome::Unavailable));
    }

    #[test]
    fn renderer_reports_the_actual_start_result_after_initialization() {
        let source = include_str!("render.rs");
        let start = source.split_once("RecordingCommand::Start {").unwrap().1;
        assert!(
            start.find("let result = self.recording.start(").unwrap()
                < start.find("reporter.complete(result.is_ok())").unwrap()
        );
        let backend = include_str!("../backend.rs");
        let tracked = backend
            .split_once("fn compositor_request_recording_region_tracked(")
            .unwrap()
            .1
            .split_once("fn compositor_set_recording_region(")
            .unwrap()
            .0;
        assert!(tracked.contains("recording_start_channel()"));
        assert!(tracked.contains("start_recording_region_tracked(path, region, reporter)"));
        assert!(tracked.contains("self.request_render()"));
        assert!(tracked.contains("Ok(Some(receipt))"));
    }

    #[test]
    fn first_tracked_stop_keeps_the_exact_writer_while_duplicates_are_unknown() {
        use crate::backend::recording_completion::{
            RecordingStopObservations, WriterFailure, recording_stop_channel,
            writer_completion_channel,
        };
        let mut pending = PendingRecordingCommands::default();
        let (first, first_receipt) = recording_stop_channel();
        pending.stop_tracked(first);
        pending.stop(); // A legacy stop cannot displace the owned observer.
        let (second, second_receipt) = recording_stop_channel();
        pending.stop_tracked(second);
        let mut first_observer = RecordingStopObservations::default();
        let mut second_observer = RecordingStopObservations::default();
        first_observer.observe(Some(first_receipt));
        second_observer.observe(Some(second_receipt));
        assert!(second_observer.take_notices().unknown);
        assert!(!first_observer.take_notices().unknown);
        let mut ready = pending.take().into_iter().flatten();
        let stop = ready.next().unwrap();
        assert_eq!(stop.command, RecordingCommand::Stop);
        assert!(ready.next().is_none());
        let (writer, completion) = writer_completion_channel();
        stop.stop_reporter.unwrap().complete(Some(completion));
        assert!(!first_observer.take_notices().failed);
        writer.finish(Some(WriterFailure::FrameWrite), None);
        let notices = first_observer.take_notices();
        assert!(notices.failed);
        assert!(!notices.unknown);
        assert!(!second_observer.take_notices().failed);
    }

    #[test]
    fn stop_start_stop_start_keeps_stop_and_start_receipts_with_their_owners() {
        use crate::backend::recording_admission::{RecordingStartOutcome, recording_start_channel};
        use crate::backend::recording_completion::{
            RecordingStopObservations, recording_stop_channel,
        };
        let mut pending = PendingRecordingCommands::default();
        let (first, first_receipt) = recording_stop_channel();
        pending.stop_tracked(first);
        let (canceled, mut canceled_receipt) = recording_start_channel();
        pending.start_tracked("canceled.mp4".into(), REGION, Some(canceled));
        let (second, second_receipt) = recording_stop_channel();
        pending.stop_tracked(second);
        let (new, mut new_receipt) = recording_start_channel();
        pending.start_tracked("new.mp4".into(), REGION, Some(new));
        assert_eq!(
            canceled_receipt.poll(),
            Some(RecordingStartOutcome::Unavailable)
        );
        let mut first_observer = RecordingStopObservations::default();
        let mut second_observer = RecordingStopObservations::default();
        first_observer.observe(Some(first_receipt));
        second_observer.observe(Some(second_receipt));
        assert!(second_observer.take_notices().unknown);
        let commands: Vec<_> = pending.take().into_iter().flatten().collect();
        assert_eq!(commands.len(), 2);
        for ready in commands {
            match ready.command {
                RecordingCommand::Stop => ready.stop_reporter.unwrap().complete(None),
                RecordingCommand::Start { path, .. } => {
                    assert_eq!(path, "new.mp4");
                    assert!(ready.stop_reporter.is_none());
                    ready.start_reporter.unwrap().complete(true);
                }
            }
        }
        assert!(first_observer.take_notices().unknown);
        assert_eq!(new_receipt.poll(), Some(RecordingStartOutcome::Accepted));
        assert!(pending.take().iter().all(Option::is_none));
    }

    #[test]
    fn a_dropped_pending_stop_is_unknown_and_render_delivery_uses_the_actual_sink() {
        use crate::backend::recording_completion::{
            RecordingStopObservations, recording_stop_channel,
        };
        let (reporter, receipt) = recording_stop_channel();
        let mut pending = PendingRecordingCommands::default();
        pending.stop_tracked(reporter);
        drop(pending);
        let mut observer = RecordingStopObservations::default();
        observer.observe(Some(receipt));
        assert!(observer.take_notices().unknown);
        let render = include_str!("render.rs");
        let stop = render
            .split_once("RecordingCommand::Stop =>")
            .unwrap()
            .1
            .split_once("RecordingCommand::Start")
            .unwrap()
            .0;
        assert!(stop.contains("let writer = unsafe { self.recording.stop_tracked(gl) }"));
        assert!(stop.contains("reporter.complete(writer)"));
    }

    #[test]
    fn pending_commands_keep_both_render_admission_paths_live() {
        let source = include_str!("mod.rs");
        for (start, end) in [
            (
                "pub(crate) fn needs_render(&self)",
                "pub(crate) fn next_wakeup",
            ),
            (
                "pub(crate) fn recording_requires_composition",
                "pub(crate) fn set_mic_indicator",
            ),
        ] {
            let body = source
                .split_once(start)
                .unwrap()
                .1
                .split_once(end)
                .unwrap()
                .0;
            assert!(body.contains("self.pending_recording.has_pending()"));
        }
        let render = include_str!("render.rs");
        assert!(
            render.contains(
                "let recording_transition_pending = self.pending_recording.has_pending();"
            )
        );
    }

    #[test]
    fn renderer_applies_commands_only_after_the_current_capture_snapshot() {
        let source = include_str!("render.rs");
        let section = source
            .split_once("// 21. Recording capture (async PBO readback to ffmpeg)")
            .unwrap()
            .1
            .split_once("// The crop outline and the REC chip")
            .unwrap()
            .0;
        let capture = section.find("self.recording.capture_frame(").unwrap();
        let commands = section.find("self.pending_recording.take()").unwrap();
        let stop = section.find("self.recording.stop_tracked(gl)").unwrap();
        let start = section.find("self.recording.start(").unwrap();
        assert!(capture < commands && commands < stop && stop < start);
    }
}
