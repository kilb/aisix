use aisix_gateway::sse::{SseDecoder, SseEvent, SseFrameSplitter};

#[test]
fn frame_delivery_preserves_coalesced_bytes_partial_tail_and_every_chunk_cut() {
    // 不同换行、UTF-8 跨片、多 data 行和尾帧来自同一条可读协议会话。
    let wire =
        "data: first\n\nevent: update\r\ndata: 你好\r\ndata: world\r\n\r\ndata: last\r\rdata: tail"
            .as_bytes();
    for chunk_bytes in 1..=wire.len() {
        let mut splitter = SseFrameSplitter::new(128);
        let mut decoded = SseDecoder::with_max_frame_bytes(128);
        let mut forwarded = Vec::new();
        let mut events = Vec::new();
        for chunk in wire.chunks(chunk_bytes) {
            splitter.push(chunk);
            while let Some(frame) = splitter.next_frame().unwrap() {
                forwarded.extend_from_slice(&frame);
            }
            events.extend(decoded.feed(chunk).unwrap());
        }
        let tail = splitter.take_rest();
        assert_eq!(tail, b"data: tail", "chunk_bytes={chunk_bytes}");
        forwarded.extend_from_slice(&tail);
        assert_eq!(forwarded, wire, "chunk_bytes={chunk_bytes}");
        events.extend(decoded.finish().unwrap());
        assert_eq!(
            events,
            [
                SseEvent::Data("first".into()),
                SseEvent::Data("你好\nworld".into()),
                SseEvent::Data("last".into()),
                SseEvent::Data("tail".into()),
            ],
            "chunk_bytes={chunk_bytes}"
        );
    }
}

#[test]
fn many_complete_frames_do_not_spend_the_incomplete_frame_budget() {
    // 限额属于尚未结束的一帧，不属于整个响应；前缀消费后仍须接受后续片段。
    let mut splitter = SseFrameSplitter::new(8);
    let body = b"data:a\n\n".repeat(4096);
    for _ in 0..4 {
        splitter.push(&body);
        let mut frames = 0;
        while let Some(frame) = splitter.next_frame().unwrap() {
            assert_eq!(frame, b"data:a\n\n");
            frames += 1;
        }
        assert_eq!(frames, 4096);
        assert!(splitter.buffered().is_empty());
    }
    splitter.push(b"12345678");
    assert!(splitter.next_frame().unwrap().is_none());
    splitter.push(b"9");
    assert_eq!(splitter.next_frame().unwrap_err().limit, 8);
    assert_eq!(splitter.take_rest(), b"123456789");
    splitter.push(b"data:b\n\n");
    assert_eq!(splitter.next_frame().unwrap(), Some(b"data:b\n\n".to_vec()));
    assert!(splitter.next_frame().unwrap().is_none());
}
