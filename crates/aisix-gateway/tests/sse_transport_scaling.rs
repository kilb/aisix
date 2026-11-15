use std::hint::black_box;
use std::time::Instant;

use aisix_gateway::sse::{SseFrameSplitter, MAX_SSE_FRAME_BYTES};

fn wire(frames: usize, frame_bytes: usize) -> Vec<u8> {
    let mut frame = b"data: ".to_vec();
    frame.resize(frame_bytes - 2, b'x');
    frame.extend_from_slice(b"\n\n");
    frame.repeat(frames)
}

fn assert_exact_delivery(body: &[u8], frames: usize, chunk_bytes: usize) {
    let mut parser = SseFrameSplitter::new(MAX_SSE_FRAME_BYTES);
    let mut forwarded = Vec::new();
    let mut count = 0;
    for chunk in body.chunks(chunk_bytes) {
        parser.push(chunk);
        while let Some(frame) = parser.next_frame().unwrap() {
            forwarded.extend_from_slice(&frame);
            count += 1;
        }
    }
    assert_eq!(count, frames);
    assert_eq!(forwarded, body);
    assert!(parser.buffered().is_empty());
    assert!(parser.take_rest().is_empty());
}

fn consume(body: &[u8], chunk_bytes: usize) -> (usize, usize, usize) {
    let mut parser = SseFrameSplitter::new(MAX_SSE_FRAME_BYTES);
    let mut count = 0;
    let mut bytes = 0;
    for chunk in body.chunks(chunk_bytes) {
        parser.push(black_box(chunk));
        while let Some(frame) = parser.next_frame().unwrap() {
            count += 1;
            bytes += black_box(frame).len();
        }
    }
    (count, bytes, black_box(parser.take_rest()).len())
}

fn probe(case: &str, frames: usize, frame_bytes: usize, chunk_frames: usize, repetitions: usize) {
    let body = wire(frames, frame_bytes);
    let chunk_bytes = chunk_frames * frame_bytes;
    // 完整字节等价在计时外检查；删除帧、重复帧或留下未消费尾部均须失败。
    assert_exact_delivery(&body, frames, chunk_bytes);
    let mut samples = Vec::new();
    for sample in 0..7 {
        let mut counts = (0usize, 0usize, 0usize);
        let start = Instant::now();
        for _ in 0..repetitions {
            let received = black_box(consume(black_box(&body), chunk_bytes));
            counts.0 += received.0;
            counts.1 += received.1;
            counts.2 += received.2;
        }
        let elapsed = start.elapsed().as_secs_f64() * 1e9 / repetitions as f64;
        assert_eq!(counts, (frames * repetitions, body.len() * repetitions, 0));
        samples.push(elapsed);
        println!(
            "SSE_SPLITTER_SAMPLE {{\"case\":\"{case}\",\"frames\":{frames},\"frame_bytes\":{frame_bytes},\"response_bytes\":{},\"chunk_frames\":{chunk_frames},\"repetitions\":{repetitions},\"sample\":{sample},\"ns_per_response\":{elapsed}}}",
            body.len()
        );
    }
    let mut ordered = samples.clone();
    ordered.sort_by(f64::total_cmp);
    println!(
        "SSE_SPLITTER_SUMMARY {{\"case\":\"{case}\",\"frames\":{frames},\"frame_bytes\":{frame_bytes},\"response_bytes\":{},\"chunk_frames\":{chunk_frames},\"repetitions\":{repetitions},\"median_ns_per_response\":{},\"samples_ns_per_response\":{:?}}}",
        body.len(), ordered[3], samples
    );
}

#[test]
fn splitter_small_response_scaling_probe() {
    // 常见小响应单独计量，不能用合并大响应的收益掩盖额外常数成本。
    let mut cases = 0;
    for frames in [1usize, 2, 8, 64] {
        for frame_bytes in [16usize, 64, 128] {
            let mut chunk_frames = vec![1, frames];
            chunk_frames.dedup();
            for chunk_frames in chunk_frames {
                probe(
                    "small",
                    frames,
                    frame_bytes,
                    chunk_frames,
                    (32768 / frames).max(512),
                );
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 21);
}

#[test]
fn splitter_512k_coalescence_scaling_probe() {
    // 响应始终是同一份 512 KiB；只改变每次 push 中包含的完整帧数。
    for chunk_frames in [1usize, 16, 128, 1024, 4096] {
        probe("512k", 4096, 128, chunk_frames, 8);
    }
}
