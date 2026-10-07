fn main() {
    let bytes = std::fs::read("/tmp/opencode/tone.wav").unwrap();
    let e = am_playback::audio::NativeEngine::new().unwrap();
    let dur = e.play_bytes("test-track-1".into(), &bytes).unwrap();
    println!("duration_ms={dur}");
    for i in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let s = e.status();
        println!(
            "t{i}: playing={} track={:?} pos={} dur={}",
            s.playing, s.track_id, s.position_ms, s.duration_ms
        );
    }
    e.pause().unwrap();
    let s = e.status();
    println!("paused: playing={} pos={}", s.playing, s.position_ms);
    e.resume().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    let s = e.status();
    println!("resumed: playing={} pos={}", s.playing, s.position_ms);
    e.seek(3000).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    let s = e.status();
    println!("seeked: playing={} pos={}", s.playing, s.position_ms);
}
