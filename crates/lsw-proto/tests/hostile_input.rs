//! Deterministic randomized robustness test for every decoder of guest
//! data, run with the normal test suite (`fuzz/` holds the coverage-guided
//! cargo-fuzz targets for the same entry points). No input may panic,
//! allocate unboundedly or loop.

use lsw_proto::coherence::{CoMessage, apply_tile};
use lsw_proto::{
    ClipFormat, ClipboardAssembler, Decoder, Envelope, Message, encode, encode_message,
};

/// xorshift64*: reproducible without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn seeds() -> Vec<Vec<u8>> {
    let control = [
        Message::Hello {
            protocol: 1,
            agent_version: "v".into(),
            os_build: "b".into(),
        },
        Message::ClipboardData {
            serial: 1,
            format: ClipFormat::Png,
            total: 10,
            offset: 0,
            chunk: vec![1; 10],
        },
        Message::DisplayHint {
            width: 800,
            height: 600,
            scale_percent: 100,
        },
        Message::Error {
            code: lsw_proto::ErrorCode::Internal,
            detail: "x".into(),
        },
    ];
    let mut out: Vec<Vec<u8>> = control
        .into_iter()
        .map(|m| encode(&Envelope::new(1, m)).unwrap())
        .collect();
    let tiles = lsw_proto::coherence::damaged_tiles(&vec![7u8; 64 * 64 * 4], None, 64, 64);
    out.push(
        encode_message(&CoMessage::Frame {
            id: 1,
            width: 64,
            height: 64,
            tiles,
        })
        .unwrap(),
    );
    out
}

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..=rng.below(8) {
        match rng.below(4) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = rng.byte();
            }
            1 => {
                let i = rng.below(v.len() + 1);
                v.insert(i, rng.byte());
            }
            2 if !v.is_empty() => {
                v.truncate(rng.below(v.len()));
            }
            _ => v.extend((0..rng.below(16)).map(|_| rng.byte())),
        }
    }
    v
}

#[test]
fn decoders_survive_mutated_and_random_input() {
    let mut rng = Rng(0x5eed_1234_dead_beef);
    let seeds = seeds();
    for round in 0..100_000 {
        let input = if round % 4 == 0 {
            (0..rng.below(256)).map(|_| rng.byte()).collect()
        } else {
            {
                let i = rng.below(seeds.len());
                mutate(&mut rng, &seeds[i])
            }
        };
        // Control/session channel.
        let mut d = Decoder::<Envelope>::new();
        if d.push(&input).is_ok() {
            while let Ok(Some(env)) = d.next_envelope() {
                if let Message::ClipboardData {
                    serial,
                    format,
                    total,
                    offset,
                    chunk,
                } = env.msg
                {
                    let mut asm = ClipboardAssembler::default();
                    let _ = asm.push(serial, format, total, offset, &chunk);
                }
            }
        }
        // Coherence channel, including tile decompression into a frame.
        let mut d = Decoder::<CoMessage>::new();
        if d.push(&input).is_ok() {
            while let Ok(Some(msg)) = d.next_message() {
                if let CoMessage::Frame {
                    width,
                    height,
                    tiles,
                    ..
                } = msg
                {
                    // Validation passed, so the frame size is bounded.
                    let mut frame = vec![0u8; width as usize * height as usize * 4];
                    for t in &tiles {
                        let _ = apply_tile(&mut frame, width, t);
                    }
                }
            }
        }
    }
}
