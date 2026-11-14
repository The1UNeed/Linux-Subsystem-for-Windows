#![no_main]
use libfuzzer_sys::fuzz_target;
use lsw_proto::Decoder;
use lsw_proto::coherence::{CoMessage, apply_tile};

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::<CoMessage>::new();
    if d.push(data).is_err() {
        return;
    }
    while let Ok(Some(msg)) = d.next_message() {
        if let CoMessage::Frame { width, height, tiles, .. } = msg {
            let mut frame = vec![0u8; width as usize * height as usize * 4];
            for t in &tiles {
                let _ = apply_tile(&mut frame, width, t);
            }
        }
    }
});
