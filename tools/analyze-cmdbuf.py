"""Parses a command-buffer dump written by the layer (XB1_DUMP_CMD=<dir>) as PM4 packets and
prints how many packets of each kind it holds.

    python tools/analyze-cmdbuf.py cmd_flush_0.bin

Headers are read AMD-PM4 style: bits 31:30 = type; type 0 = register writes (bits 29:16 = count - 1,
bits 15:0 = first register); type 3 = opcode packets (bits 15:8 = opcode); type 2 = one-dword NOP.
The title's stream so far parses exactly (no desync) under this reading.
"""
import collections
import struct
import sys

data = open(sys.argv[1], 'rb').read()
n = len(data) // 4
d = struct.unpack('<%dI' % n, data[:n * 4])
pos, stats, firsts = 0, collections.Counter(), {}
while pos < n:
    h = d[pos]
    t = h >> 30
    if t == 0:
        cnt = ((h >> 16) & 0x3FFF) + 1
        key, total = ('type0', hex(h & 0xFFFF), cnt), 1 + cnt
    elif t == 3:
        cnt = ((h >> 16) & 0x3FFF) + 1
        key, total = ('type3', hex((h >> 8) & 0xFF), cnt), 1 + cnt
    elif t == 2:
        key, total = ('type2',), 1
    else:
        key, total = ('type1', hex(h)), 2
    if pos + total > n:
        print('packet runs past the end at dword', pos)
        break
    stats[key] += 1
    firsts.setdefault(key, [hex(x) for x in d[pos:pos + min(total, 12)]])
    pos += total
print('parsed', pos, 'of', n, 'dwords')
for key, count in stats.most_common(40):
    print(count, key, firsts[key])
