#!/usr/bin/env python3
"""Outil de TEST/DEBUG : fabrique ou décode une entrée `otadata` Embewi (EWBT).

Ne fait PAS partie du build ni de l'image de production : `embewi-boot` est le
seul propriétaire de l'amorçage d'`otadata` (otadata vierge -> il valide ota_0,
écrit Valid(seq=1) et relit, puis boote). Cet outil sert à fabriquer un état
précis pour un test (par ex. un `Pending` pour éprouver le rollback), ou à lire
ce qu'un device a écrit.

Format (identique à crates/embewi-boot-core, figé par ses vecteurs « golden »),
32 octets par entrée, une par secteur de 4 Ko :
  0 seq u32 | 4 "EWBT" | 8 version u32=1 | 12 ext_crc u32 | 16 réservé (effacé)
  | 20 mot de commit u32 | 24 état u32 | 28 crc IDF de seq
ext_crc = crc32(seq, état, magic, version) ; états : 0 New, 1 Pending, 2 Valid,
3 Invalid, 4 Aborted.

Usage :
  ewbt-otadata.py make <seq> <état> [--no-commit]     # écrit 32 octets hex
  ewbt-otadata.py decode <hex|fichier> [offset]        # lit un secteur/une image
  ewbt-otadata.py image <image> <offset-otadata> <seq0>:<état0> <seq1>:<état1>
      # écrit deux entrées dans une image mergée ('-' = secteur vierge)
"""
import struct
import sys
import zlib

MAGIC = b"EWBT"
VERSION = 1
COMMIT = 0x5AC3A53C
STATES = {"new": 0, "pending": 1, "valid": 2, "invalid": 3, "aborted": 4}
NAMES = {v: k for k, v in STATES.items()}
SECTOR = 0x1000


def crc(data, init=0xFFFFFFFF):
    return zlib.crc32(data, init) & 0xFFFFFFFF


def make(seq, state, commit=True):
    ext = crc(struct.pack("<II", seq, state) + MAGIC + struct.pack("<I", VERSION))
    raw = bytearray(b"\xff" * 32)
    raw[0:4] = struct.pack("<I", seq)
    raw[4:8] = MAGIC
    raw[8:12] = struct.pack("<I", VERSION)
    raw[12:16] = struct.pack("<I", ext)
    if commit:
        raw[20:24] = struct.pack("<I", COMMIT)
    raw[24:28] = struct.pack("<I", state)
    raw[28:32] = struct.pack("<I", crc(struct.pack("<I", seq)))
    return bytes(raw)


def decode(raw):
    if raw == b"\xff" * 32:
        return "blank"
    seq, state = struct.unpack_from("<I", raw, 0)[0], struct.unpack_from("<I", raw, 24)[0]
    if state in NAMES and seq not in (0, 0xFFFFFFFF) and raw == make(seq, state):
        return f"seq={seq} state={NAMES[state]} (committed)"
    if state in NAMES and raw == make(seq, state, commit=False):
        return f"seq={seq} state={NAMES[state]} (body written, NOT committed: ignored by the bootloader)"
    return "corrupt (not an Embewi entry; ignored by the bootloader)"


def parse_state(text):
    return STATES[text] if text in STATES else int(text, 0)


def main(argv):
    if len(argv) >= 4 and argv[1] == "make":
        print(make(int(argv[2], 0), parse_state(argv[3]), "--no-commit" not in argv).hex())
    elif len(argv) >= 3 and argv[1] == "decode":
        try:
            data = open(argv[2], "rb").read()
            off = int(argv[3], 0) if len(argv) > 3 else 0
            for i in range(2):
                print(f"sector {i}: {decode(data[off + i * SECTOR:off + i * SECTOR + 32])}")
        except FileNotFoundError:
            print(decode(bytes.fromhex(argv[2])))
    elif len(argv) == 6 and argv[1] == "image":
        image, off = argv[2], int(argv[3], 0)
        data = bytearray(open(image, "rb").read())
        for i, spec in enumerate(argv[4:6]):
            sector = b"\xff" * SECTOR
            if spec != "-":
                seq, state = spec.split(":")
                sector = make(int(seq, 0), parse_state(state)) + b"\xff" * (SECTOR - 32)
            data[off + i * SECTOR:off + (i + 1) * SECTOR] = sector
        open(image, "wb").write(data)
        print(f"otadata écrit dans {image} @ {off:#x}")
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv)
