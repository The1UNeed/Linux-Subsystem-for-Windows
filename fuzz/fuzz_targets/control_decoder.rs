#![no_main]
use libfuzzer_sys::fuzz_target;
use lsw_proto::{ClipboardAssembler, Decoder, Envelope, Message};

fuzz_target!(|data: &[u8]| {
    let mut d = Decoder::<Envelope>::new();
    for chunk in data.chunks(97) {
        if d.push(chunk).is_err() {
            return;
        }
        while let Ok(Some(env)) = d.next_envelope() {
            if let Message::ClipboardData { serial, format, total, offset, chunk } = env.msg {
                let _ = ClipboardAssembler::default().push(serial, format, total, offset, &chunk);
            }
        }
    }
});
