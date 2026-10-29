//! Local feedback shares the real playback completion boundary with cloud audio.
pub async fn tone(succeeded: bool, labels: serde_json::Value) -> std::io::Result<()> {
    let frequency = if succeeded { 880.0 } else { 440.0 };
    let mut pcm = Vec::with_capacity(4800);
    for index in 0..2400 {
        let envelope = (index as f32 / 240.0)
            .min((2400 - index) as f32 / 240.0)
            .min(1.0);
        let sample = ((std::f32::consts::TAU * frequency * index as f32 / 24000.0).sin()
            * envelope
            * 4000.0) as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let mut output = crate::audio::Playback::start()?;
    output.set_telemetry(labels);
    output.write(&pcm).await?;
    output.finish().await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "requires the private native PipeWire graph from tests/native-audio.sh"]
    async fn native_feedback_tone_finishes_at_real_drain() {
        assert_eq!(std::env::var("JWM_EVA_AUDIO_TEST").as_deref(), Ok("1"));
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            super::tone(
                true,
                serde_json::json!({"generation":1,"turn_id":2,"phase":"local_tone"}),
            ),
        )
        .await
        .expect("local feedback must not remain in Responding")
        .expect("native tone must drain successfully");
    }
}
