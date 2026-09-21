#!/usr/bin/env python3
"""Écrit un `otadata` initial (seq=1 -> ota_0 courant, état VALID) dans une image
mergée et dans un fichier autonome (pour flasher `otadata` seul).

Format (esp_ota_select_entry_t, 32 octets) : ota_seq u32 | seq_label[20] 0xFF |
ota_state u32 (2 = VALID) | crc u32 = crc32_le(0xFFFFFFFF, ota_seq). Deux secteurs
de 4 Ko : le premier porte l'entrée, le second reste vierge. Même contenu que celui
que le bootloader ESP-IDF écrivait au premier boot ; le CRC est celui du vecteur de
test d'esp-bootloader-esp-idf (SLOT_COUNT_1_VALID).

Usage: otadata-seed.py <image-mergée> <offset-otadata> <sortie-otadata.bin>
"""
import struct
import sys
import zlib

image, offset, out = sys.argv[1], int(sys.argv[2], 0), sys.argv[3]
seq = struct.pack("<I", 1)
entry = seq + b"\xff" * 20 + struct.pack("<I", 2) + struct.pack("<I", zlib.crc32(seq, 0xFFFFFFFF))
blob = entry + b"\xff" * (0x1000 - len(entry)) + b"\xff" * 0x1000
assert len(entry) == 32 and len(blob) == 0x2000

data = bytearray(open(image, "rb").read())
if len(data) < offset + len(blob):
    sys.exit("image trop courte pour contenir otadata")
data[offset:offset + len(blob)] = blob
open(image, "wb").write(data)
open(out, "wb").write(blob)
print(f"otadata seedé (seq=1, VALID) à {offset:#x} dans {image} et écrit dans {out}")
