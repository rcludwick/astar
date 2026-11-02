//! astar addition (see VENDORED.md): LAME, built by the patched `cc` build,
//! produces valid MPEG-2 / 2.5 Layer III frames at both pipeline rates.
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, MonoPcm, Quality};

fn encode_tone(rate: u32) -> Vec<u8> {
    let mut b = Builder::new().expect("lame init");
    b.set_num_channels(1).unwrap();
    b.set_sample_rate(rate).unwrap();
    b.set_brate(Bitrate::Kbps32).unwrap();
    b.set_quality(Quality::Good).unwrap();
    let mut enc = b.build().unwrap();
    let pcm: Vec<i16> = (0..rate as usize)
        .map(|i| ((i as f32 * 1000.0 * std::f32::consts::TAU / rate as f32).sin() * 8000.0) as i16)
        .collect();
    let mut out = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    let n = enc.encode(MonoPcm(&pcm), out.spare_capacity_mut()).unwrap();
    unsafe { out.set_len(n) };
    let mut tail = Vec::with_capacity(7200);
    let m = enc.flush::<FlushNoGap>(tail.spare_capacity_mut()).unwrap();
    unsafe { tail.set_len(m) };
    out.extend(tail);
    out
}

#[test]
fn one_second_of_tone_is_valid_mpeg_frames_at_8k_and_16k() {
    for rate in [8_000, 16_000] {
        let mp3 = encode_tone(rate);
        assert!(mp3.len() > 1000, "rate {rate}: {} bytes", mp3.len());
        let i = mp3
            .windows(2)
            .position(|w| w[0] == 0xFF && w[1] & 0xE0 == 0xE0)
            .expect("frame sync");
        // MPEG-2 (16 kHz) or MPEG-2.5 (8 kHz), never MPEG-1.
        assert_ne!(mp3[i + 1] & 0x18, 0x18, "rate {rate} encoded as MPEG-1");
    }
}
