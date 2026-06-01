from __future__ import annotations

import gzip
import struct


class ProtobufEncoder:
    def __init__(self) -> None:
        self._chunks: list[bytes] = []

    def _varint(self, value: int) -> bytes:
        if value == 0:
            return b"\x00"
        bytes_list: list[int] = []
        while value > 0x7F:
            bytes_list.append((value & 0x7F) | 0x80)
            value >>= 7
        bytes_list.append(value & 0x7F)
        return bytes(bytes_list)

    def _tag(self, field: int, wire: int) -> bytes:
        return self._varint((field << 3) | wire)

    def write_varint(self, field: int, value: int) -> ProtobufEncoder:
        self._chunks.append(self._tag(field, 0))
        self._chunks.append(self._varint(value))
        return self

    def write_string(self, field: int, value: str) -> ProtobufEncoder:
        data = value.encode("utf-8")
        self._chunks.append(self._tag(field, 2))
        self._chunks.append(self._varint(len(data)))
        self._chunks.append(data)
        return self

    def write_bytes(self, field: int, value: bytes) -> ProtobufEncoder:
        self._chunks.append(self._tag(field, 2))
        self._chunks.append(self._varint(len(value)))
        self._chunks.append(value)
        return self

    def write_message(self, field: int, sub: ProtobufEncoder) -> ProtobufEncoder:
        data = sub.to_bytes()
        self._chunks.append(self._tag(field, 2))
        self._chunks.append(self._varint(len(data)))
        self._chunks.append(data)
        return self

    def to_bytes(self) -> bytes:
        return b"".join(self._chunks)


def decode_varint(buf: bytes, offset: int) -> tuple[int, int]:
    value = 0
    shift = 0
    while offset < len(buf):
        b = buf[offset]
        offset += 1
        value |= (b & 0x7F) << shift
        shift += 7
        if not (b & 0x80):
            break
    return value, offset


def extract_strings(data: bytes) -> list[str]:
    strings: list[str] = []
    i = 0
    while i < len(data):
        tag = 0
        shift = 0
        while i < len(data):
            b = data[i]
            i += 1
            tag |= (b & 0x7F) << shift
            shift += 7
            if not (b & 0x80):
                break
        wire = tag & 0x7
        if wire == 0:
            while i < len(data):
                b = data[i]
                i += 1
                if not (b & 0x80):
                    break
        elif wire == 1:
            i += 8
        elif wire == 2:
            length = 0
            shift = 0
            while i < len(data):
                b = data[i]
                i += 1
                length |= (b & 0x7F) << shift
                shift += 7
                if not (b & 0x80):
                    break
            if i + length <= len(data):
                raw = data[i : i + length]
                try:
                    text = raw.decode("utf-8")
                    if len(text) > 5:
                        strings.append(text)
                except UnicodeDecodeError:
                    pass
            i += length
        elif wire == 5:
            i += 4
        else:
            break
    return strings


def connect_frame_encode(proto_bytes: bytes, compress: bool = True) -> bytes:
    if compress:
        payload = gzip.compress(proto_bytes)
        flags = 1
    else:
        payload = proto_bytes
        flags = 0
    header = struct.pack(">B I", flags, len(payload))
    return header + payload


def connect_frame_decode(data: bytes) -> list[bytes]:
    frames: list[bytes] = []
    i = 0
    while i + 5 <= len(data):
        flags, length = struct.unpack_from(">B I", data, i)
        i += 5
        payload = data[i : i + length]
        i += length
        if flags in (1, 3):
            try:
                payload = gzip.decompress(payload)
            except Exception:
                pass
        frames.append(payload)
    return frames
