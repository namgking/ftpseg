"""Verify RT_ICON and RT_GROUP_ICON resources in generated PE executables."""
from pathlib import Path
import struct
root = Path(__file__).resolve().parents[1]
for name in ['ftpseg.exe', 'ftpseg-gui.exe']:
    p = root / 'target/release' / name
    data = p.read_bytes()
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    assert data[pe:pe+4] == b'PE\0\0'
    sections = struct.unpack_from('<H', data, pe+6)[0]
    optsize = struct.unpack_from('<H', data, pe+20)[0]
    opt = pe+24
    magic = struct.unpack_from('<H', data, opt)[0]
    dd = opt + (112 if magic == 0x20b else 96)
    rva, size = struct.unpack_from('<II', data, dd+16)
    assert rva and size, f'{name}: no resources'
    table = opt+optsize
    def offset(address):
        for i in range(sections):
            s = table+i*40
            virtual_size, va, raw_size, raw = struct.unpack_from('<IIII', data, s+8)
            if va <= address < va+max(virtual_size,raw_size):
                return raw+address-va
        raise AssertionError('unmapped RVA')
    base = offset(rva)
    def entries(relative):
        directory = base+relative
        named, ids = struct.unpack_from('<HH',data,directory+12)
        return [struct.unpack_from('<II',data,directory+16+i*8) for i in range(named+ids)]
    types = dict(entries(0))
    assert 3 in types and 14 in types, f'{name}: missing icon resource types'
    group_ids = entries(types[14] & 0x7fffffff)
    languages = entries(group_ids[0][1] & 0x7fffffff)
    entry = base+(languages[0][1] & 0x7fffffff)
    payload_rva, length = struct.unpack_from('<II',data,entry)
    group = offset(payload_rva)
    count = struct.unpack_from('<H',data,group+4)[0]
    assert count == 7, (name,count)
    print(f'{name}: RT_ICON + RT_GROUP_ICON verified; {count} sizes embedded; {p.stat().st_size} bytes')
