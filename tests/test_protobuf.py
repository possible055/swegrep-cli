from swegrep_cli.protobuf import (
    ProtobufEncoder,
    connect_frame_decode,
    connect_frame_encode,
    decode_varint,
    extract_strings,
)


def test_varint_encode_decode() -> None:
    encoder = ProtobufEncoder()
    # Test simple values
    assert encoder._varint(0) == b"\x00"
    assert encoder._varint(1) == b"\x01"
    assert encoder._varint(127) == b"\x7f"
    assert encoder._varint(128) == b"\x80\x01"
    assert encoder._varint(300) == b"\xac\x02"

    # Test decode
    val, offset = decode_varint(b"\x00", 0)
    assert val == 0
    assert offset == 1

    val, offset = decode_varint(b"\xac\x02", 0)
    assert val == 300
    assert offset == 2


def test_protobuf_encoder() -> None:
    encoder = ProtobufEncoder()
    encoder.write_varint(1, 150)
    # Tag for field 1 wire 0: (1 << 3) | 0 = 8. Varint 150: 0x96 0x01
    assert encoder.to_bytes() == b"\x08\x96\x01"

    encoder2 = ProtobufEncoder()
    encoder2.write_string(2, "hello")
    # Tag for field 2 wire 2: (2 << 3) | 2 = 18 (0x12). Length 5. "hello"
    assert encoder2.to_bytes() == b"\x12\x05hello"

    encoder3 = ProtobufEncoder()
    encoder3.write_bytes(3, b"\x01\x02")
    # Tag for field 3 wire 2: 26 (0x1a). Length 2
    assert encoder3.to_bytes() == b"\x1a\x02\x01\x02"

    encoder4 = ProtobufEncoder()
    sub = ProtobufEncoder().write_varint(1, 10)
    encoder4.write_message(4, sub)
    # Tag for field 4 wire 2: 34 (0x22). Length 2. Content 0x08 0x0a
    assert encoder4.to_bytes() == b"\x22\x02\x08\x0a"


def test_connect_frame_encode_decode() -> None:
    data = b"my test protobuf payload"

    # Test compressed
    frame = connect_frame_encode(data, compress=True)
    assert frame[0] == 1
    decoded = connect_frame_decode(frame)
    assert len(decoded) == 1
    assert decoded[0] == data

    # Test uncompressed
    frame_uncompressed = connect_frame_encode(data, compress=False)
    assert frame_uncompressed[0] == 0
    decoded_uncompressed = connect_frame_decode(frame_uncompressed)
    assert len(decoded_uncompressed) == 1
    assert decoded_uncompressed[0] == data


def test_extract_strings() -> None:
    encoder = ProtobufEncoder()
    encoder.write_string(1, "short")  # length 5 -> should be ignored because of len > 5 check
    encoder.write_string(2, "longer_string_here")  # length 18
    encoder.write_varint(3, 99999)
    encoder.write_string(4, "another_long_one")

    data = encoder.to_bytes()
    strings = extract_strings(data)
    assert "longer_string_here" in strings
    assert "another_long_one" in strings
    assert "short" not in strings
