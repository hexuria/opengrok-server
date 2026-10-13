use super::*;

/// What the box hears is masked, as a server requires of a client, at each of the three length
/// encodings, and reads back as the same bytes.
#[test]
fn what_is_sent_is_masked_and_reads_back_whole() {
    for size in [0usize, 5, 125, 126, 300, 0xFFFF, 0x1_0000] {
        let payload: Vec<u8> = (0..size).map(|i| i as u8).collect();
        let sent = binary(&payload, [1, 2, 3, 4]);
        assert_eq!(sent[0], 0x82);
        assert!(sent[1] & 0x80 != 0, "a client's frame is masked");
        let mut reader = Reader::default();
        reader.push(&sent);
        assert_eq!(
            reader.next_frame().unwrap(),
            Some(Heard::Data(payload)),
            "{size}"
        );
        assert_eq!(reader.next_frame().unwrap(), None);
    }
}

/// The box's frames are unmasked and may arrive split anywhere; a ping, a close and a pong each
/// mean what they say.
#[test]
fn the_boxs_frames_are_read_however_the_bytes_arrive() {
    let mut stream = vec![0x82, 3, b'R', b'F', b'B'];
    stream.extend([0x89, 2, b'h', b'i']);
    stream.extend([0x8A, 0]);
    stream.extend([0x88, 0]);
    let mut reader = Reader::default();
    let mut heard = Vec::new();
    for byte in stream {
        reader.push(&[byte]);
        while let Some(frame) = reader.next_frame().unwrap() {
            heard.push(frame);
        }
    }
    assert_eq!(
        heard,
        [
            Heard::Data(b"RFB".to_vec()),
            Heard::Ping(b"hi".to_vec()),
            Heard::Nothing,
            Heard::Close
        ]
    );
}

#[test]
fn a_frame_too_big_for_a_screen_is_refused() {
    let mut reader = Reader::default();
    reader.push(&[0x82, 127, 0, 0, 0, 0, 0x10, 0, 0, 0]);
    assert!(reader.next_frame().is_err());
}
