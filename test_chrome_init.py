#!/usr/bin/env python3
"""
Simulate Chrome's X11 initialization sequence and validate all replies.
This connects to Xserver and sends the same requests Chrome does,
checking each reply for protocol compliance.
"""

import socket
import struct
import sys
import time

HOST = "localhost"
PORT = 6000

def connect_x11():
    """Connect and perform X11 setup handshake."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.connect((HOST, PORT))

    # Send connection setup: byte-order, protocol version, no auth
    setup = struct.pack('<BxHHHHxx', 0x6C, 11, 0, 0, 0)  # LSB, X11.0, no auth
    s.sendall(setup)

    # Read setup reply
    header = recv_exact(s, 8)
    status = header[0]
    if status != 1:
        print(f"FAIL: Setup rejected, status={status}")
        return None, None

    # additional_data_len in 4-byte words
    add_len = struct.unpack_from('<H', header, 6)[0]
    additional = recv_exact(s, add_len * 4)
    setup_reply = header + additional

    print(f"OK: Setup reply received, {len(setup_reply)} bytes")

    # Parse key fields
    off = 8
    release = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    resource_id_base = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    resource_id_mask = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    motion_buffer_size = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    vendor_len = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    max_req_len = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    num_screens = setup_reply[off]; off += 1
    num_formats = setup_reply[off]; off += 1
    image_byte_order = setup_reply[off]; off += 1
    bitmap_bit_order = setup_reply[off]; off += 1
    bitmap_scanline_unit = setup_reply[off]; off += 1
    bitmap_scanline_pad = setup_reply[off]; off += 1
    min_keycode = setup_reply[off]; off += 1
    max_keycode = setup_reply[off]; off += 1
    off += 4  # pad

    print(f"  release={release}, resource_base=0x{resource_id_base:08x}, mask=0x{resource_id_mask:08x}")
    print(f"  vendor_len={vendor_len}, max_req_len={max_req_len}")
    print(f"  screens={num_screens}, formats={num_formats}")
    print(f"  image_byte_order={image_byte_order}, bitmap_bit_order={bitmap_bit_order}")
    print(f"  bitmap_scanline_unit={bitmap_scanline_unit}, bitmap_scanline_pad={bitmap_scanline_pad}")
    print(f"  min_keycode={min_keycode}, max_keycode={max_keycode}")

    # Check bitmap-scanline-unit validity
    if bitmap_scanline_unit not in (8, 16, 32):
        print(f"  WARNING: bitmap_scanline_unit={bitmap_scanline_unit} is non-standard!")

    # Skip vendor string
    vendor_pad = (4 - (vendor_len % 4)) % 4
    off += vendor_len + vendor_pad

    # Parse pixmap formats
    print(f"  Pixmap formats:")
    for i in range(num_formats):
        depth = setup_reply[off]
        bpp = setup_reply[off + 1]
        scanline_pad = setup_reply[off + 2]
        print(f"    format {i}: depth={depth} bpp={bpp} scanline_pad={scanline_pad}")
        off += 8

    # Parse screen
    screen_start = off
    root_window = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    default_colormap = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    white_pixel = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    black_pixel = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    current_input_masks = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    width_pixels = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    height_pixels = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    width_mm = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    height_mm = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    min_installed_maps = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    max_installed_maps = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
    root_visual = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
    backing_stores = setup_reply[off]; off += 1
    save_unders = setup_reply[off]; off += 1
    root_depth = setup_reply[off]; off += 1
    num_depths = setup_reply[off]; off += 1

    print(f"  Screen: root=0x{root_window:08x} {width_pixels}x{height_pixels} depth={root_depth}")
    print(f"  root_visual=0x{root_visual:08x} num_depths={num_depths}")

    # Parse depths and visuals
    for d in range(num_depths):
        depth_val = setup_reply[off]; off += 1
        off += 1  # pad
        num_visuals = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
        off += 4  # pad
        print(f"    depth {depth_val}: {num_visuals} visuals")
        for v in range(num_visuals):
            vid = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
            vclass = setup_reply[off]; off += 1
            bits_per_rgb = setup_reply[off]; off += 1
            colormap_entries = struct.unpack_from('<H', setup_reply, off)[0]; off += 2
            red_mask = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
            green_mask = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
            blue_mask = struct.unpack_from('<I', setup_reply, off)[0]; off += 4
            off += 4  # pad
            classes = {0:'StaticGray',1:'GrayScale',2:'StaticColor',3:'PseudoColor',4:'TrueColor',5:'DirectColor'}
            print(f"      visual 0x{vid:08x}: class={classes.get(vclass,vclass)} bpp={bits_per_rgb} entries={colormap_entries}")
            print(f"        R=0x{red_mask:08x} G=0x{green_mask:08x} B=0x{blue_mask:08x}")

            # Chrome CHECK: visual must be in allowed depths
            if vclass == 4:  # TrueColor
                if depth_val == root_depth and vid == root_visual:
                    print(f"        ROOT VISUAL FOUND in depth {depth_val} - OK")

    return s, seq_counter(resource_id_base)


class seq_counter:
    def __init__(self, resource_base):
        self.seq = 0
        self.resource_base = resource_base
        self.resource_next = 1

    def next_seq(self):
        self.seq += 1
        return self.seq

    def next_resource(self):
        rid = self.resource_base | self.resource_next
        self.resource_next += 1
        return rid


def recv_exact(s, n):
    """Receive exactly n bytes."""
    data = b''
    while len(data) < n:
        chunk = s.recv(n - len(data))
        if not chunk:
            raise ConnectionError(f"Connection closed, wanted {n} bytes, got {len(data)}")
        data += chunk
    return data


def send_request(s, opcode, data_bytes, sub_opcode=0):
    """Send an X11 request. Returns request length in 4-byte words."""
    total = 4 + len(data_bytes)
    pad = (4 - (total % 4)) % 4
    total += pad
    words = total // 4

    header = struct.pack('<BBH', opcode, sub_opcode, words)
    s.sendall(header + data_bytes + b'\x00' * pad)
    return words


def read_reply(s, timeout=5.0):
    """Read a 32-byte X11 reply/error/event, plus any additional data."""
    s.settimeout(timeout)
    try:
        header = recv_exact(s, 32)
    except socket.timeout:
        return None

    reply_type = header[0]

    if reply_type == 0:  # Error
        code = header[1]
        seq = struct.unpack_from('<H', header, 2)[0]
        major = header[10]
        minor = struct.unpack_from('<H', header, 8)[0]
        print(f"  ERROR: code={code} seq={seq} major={major} minor={minor}")
        return ('error', header)
    elif reply_type == 1:  # Reply
        seq = struct.unpack_from('<H', header, 2)[0]
        add_len = struct.unpack_from('<I', header, 4)[0]
        extra = b''
        if add_len > 0:
            extra = recv_exact(s, add_len * 4)
        return ('reply', header + extra, seq)
    else:  # Event
        print(f"  EVENT: type={reply_type}")
        return ('event', header)


def query_extension(s, sc, name):
    """Send QueryExtension and read reply."""
    name_bytes = name.encode('ascii')
    data = struct.pack('<HH', len(name_bytes), 0) + name_bytes
    pad = (4 - (len(name_bytes) % 4)) % 4
    data += b'\x00' * pad

    total = 4 + len(data)
    words = total // 4
    header = struct.pack('<BBH', 98, 0, words)
    s.sendall(header + data)
    sc.next_seq()

    result = read_reply(s)
    if result is None:
        print(f"  QueryExtension({name}): TIMEOUT")
        return None
    if result[0] == 'error':
        print(f"  QueryExtension({name}): ERROR")
        return None

    reply = result[1]
    present = reply[8]
    major_opcode = reply[9]
    first_event = reply[10]
    first_error = reply[11]

    status = "PRESENT" if present else "not present"
    print(f"  QueryExtension({name}): {status} opcode={major_opcode} event={first_event} error={first_error}")
    return (present, major_opcode, first_event, first_error) if present else None


def intern_atom(s, sc, name, only_if_exists=False):
    """Send InternAtom request."""
    name_bytes = name.encode('ascii')
    data = struct.pack('<HH', len(name_bytes), 0) + name_bytes
    pad = (4 - (len(name_bytes) % 4)) % 4
    data += b'\x00' * pad

    total = 4 + len(data)
    words = total // 4
    header = struct.pack('<BBH', 16, 1 if only_if_exists else 0, words)
    s.sendall(header + data)
    sc.next_seq()


def read_intern_atom_reply(s):
    """Read InternAtom reply."""
    result = read_reply(s)
    if result is None or result[0] != 'reply':
        return 0
    reply = result[1]
    atom = struct.unpack_from('<I', reply, 8)[0]
    return atom


def test_chrome_sequence():
    """Simulate Chrome's X11 initialization."""
    print("=== Chrome X11 Init Simulation ===")
    print()

    s, sc = connect_x11()
    if s is None:
        return

    print()
    print("--- Phase 1: Extension Queries ---")

    # Chrome queries these extensions (in order):
    extensions_to_query = [
        "Generic Event Extension",
        "BIG-REQUESTS",
        "XInputExtension",
        "XKEYBOARD",
        "SHAPE",
        "RENDER",
        "RANDR",
        "XFIXES",
        "SYNC",
        "XTEST",
        "MIT-SHM",
        "XFree86-DGA",
        "Present",
    ]

    ext_info = {}
    for name in extensions_to_query:
        info = query_extension(s, sc, name)
        if info:
            ext_info[name] = info

    print()
    print("--- Phase 2: BIG-REQUESTS Enable ---")

    if "BIG-REQUESTS" in ext_info:
        opcode = ext_info["BIG-REQUESTS"][1]
        header = struct.pack('<BBH', opcode, 0, 1)
        s.sendall(header)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            max_len = struct.unpack_from('<I', reply, 8)[0]
            print(f"  BIG-REQUESTS: max_len={max_len}")
        else:
            print(f"  BIG-REQUESTS: FAILED")

    print()
    print("--- Phase 2.5: Extension Version Queries ---")

    # SHAPE QueryVersion
    if "SHAPE" in ext_info:
        opcode = ext_info["SHAPE"][1]
        header = struct.pack('<BBH', opcode, 0, 1)
        s.sendall(header)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = struct.unpack_from('<H', reply, 8)[0]
            minor = struct.unpack_from('<H', reply, 10)[0]
            print(f"  SHAPE: v{major}.{minor}")
        else:
            print(f"  SHAPE QueryVersion: FAILED")

    # XTEST GetVersion
    if "XTEST" in ext_info:
        opcode = ext_info["XTEST"][1]
        data = struct.pack('<Bx2x', 2)  # major=2
        header = struct.pack('<BBH', opcode, 0, 2)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = reply[1]
            minor = struct.unpack_from('<H', reply, 8)[0]
            print(f"  XTEST: v{major}.{minor}")
        else:
            print(f"  XTEST GetVersion: FAILED")

    # SYNC Initialize
    if "SYNC" in ext_info:
        opcode = ext_info["SYNC"][1]
        data = struct.pack('<BB2x', 3, 1)  # major=3, minor=1
        header = struct.pack('<BBH', opcode, 0, 2)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = reply[8]
            minor = reply[9]
            print(f"  SYNC: v{major}.{minor}")
        else:
            print(f"  SYNC Initialize: FAILED")

    # XFIXES QueryVersion
    if "XFIXES" in ext_info:
        opcode = ext_info["XFIXES"][1]
        data = struct.pack('<II', 6, 0)  # major=6, minor=0
        header = struct.pack('<BBH', opcode, 0, 3)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = struct.unpack_from('<I', reply, 8)[0]
            minor = struct.unpack_from('<I', reply, 12)[0]
            print(f"  XFIXES: v{major}.{minor}")
        else:
            print(f"  XFIXES QueryVersion: FAILED")

    # RENDER QueryVersion
    if "RENDER" in ext_info:
        opcode = ext_info["RENDER"][1]
        data = struct.pack('<II', 0, 11)  # major=0, minor=11
        header = struct.pack('<BBH', opcode, 0, 3)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = struct.unpack_from('<I', reply, 8)[0]
            minor = struct.unpack_from('<I', reply, 12)[0]
            print(f"  RENDER: v{major}.{minor}")
        else:
            print(f"  RENDER QueryVersion: FAILED")

    # RANDR QueryVersion
    if "RANDR" in ext_info:
        randr_opcode = ext_info["RANDR"][1]
        data = struct.pack('<II', 1, 5)  # major=1, minor=5
        header = struct.pack('<BBH', randr_opcode, 0, 3)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = struct.unpack_from('<I', reply, 8)[0]
            minor = struct.unpack_from('<I', reply, 12)[0]
            print(f"  RANDR: v{major}.{minor}")
        else:
            print(f"  RANDR QueryVersion: FAILED")

    print()
    print("--- Phase 3: XI2 QueryVersion ---")

    if "XInputExtension" in ext_info:
        opcode = ext_info["XInputExtension"][1]
        # XIQueryVersion: sub-opcode 47, client version 2.0
        data = struct.pack('<HH', 2, 0)  # major=2, minor=0
        total = 4 + len(data)
        header = struct.pack('<BBH', opcode, 47, total // 4)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            major = struct.unpack_from('<H', reply, 8)[0]
            minor = struct.unpack_from('<H', reply, 10)[0]
            print(f"  XI2 version: {major}.{minor}")
        else:
            print(f"  XI2 QueryVersion: FAILED")

    print()
    print("--- Phase 4: XKB UseExtension ---")

    if "XKEYBOARD" in ext_info:
        xkb_opcode = ext_info["XKEYBOARD"][1]
        # UseExtension: sub-opcode 0, version 1.0
        data = struct.pack('<HH', 1, 0)  # wanted major=1, minor=0
        total = 4 + len(data)
        header = struct.pack('<BBH', xkb_opcode, 0, total // 4)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            supported = reply[1]
            major = struct.unpack_from('<H', reply, 8)[0]
            minor = struct.unpack_from('<H', reply, 10)[0]
            print(f"  XKB: supported={supported} version={major}.{minor}")
        else:
            print(f"  XKB UseExtension: FAILED")

    print()
    print("--- Phase 5: XKB GetState ---")

    if "XKEYBOARD" in ext_info:
        # GetState: sub-opcode 4, deviceSpec=0x0100 (UseCoreKbd)
        data = struct.pack('<HH', 0x0100, 0)
        total = 4 + len(data)
        header = struct.pack('<BBH', xkb_opcode, 4, total // 4)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            device_id = reply[1]
            mods = reply[8]
            print(f"  XKB GetState: device={device_id} mods=0x{mods:02x}")
            # Validate reply size
            if len(reply) != 32:
                print(f"  WARNING: GetState reply is {len(reply)} bytes, expected 32!")
        else:
            print(f"  XKB GetState: FAILED")

    print()
    print("--- Phase 6: XKB GetMap ---")

    if "XKEYBOARD" in ext_info:
        # GetMap: sub-opcode 8
        # deviceSpec(2), full(2), partial(2), firstType(1), nTypes(1),
        # firstKeySym(1), nKeySyms(1), firstKeyAction(1), nKeyActions(1),
        # firstKeyBehavior(1), nKeyBehaviors(1), virtualMods(2),
        # firstKeyExplicit(1), nKeyExplicit(1), firstModMapKey(1), nModMapKeys(1),
        # firstVModMapKey(1), nVModMapKeys(1), pad(2)
        data = struct.pack('<HHHBBBBBBBBHBBBBBBH',
            0x0100,  # deviceSpec = UseCoreKbd
            0x0007,  # full = types | syms | modmap
            0,       # partial
            0, 0,    # firstType, nTypes (ignored when full)
            8, 248,  # firstKeySym, nKeySyms (8-255)
            0, 0,    # firstKeyAction, nKeyActions
            0, 0,    # firstKeyBehavior, nKeyBehaviors
            0,       # virtualMods
            0, 0,    # firstKeyExplicit, nKeyExplicit
            0, 0,    # firstModMapKey, nModMapKeys
            0, 0,    # firstVModMapKey, nVModMapKeys
            0,       # pad
        )
        total = 4 + len(data)
        pad_len = (4 - (total % 4)) % 4
        total += pad_len
        header = struct.pack('<BBH', xkb_opcode, 8, total // 4)
        s.sendall(header + data + b'\x00' * pad_len)
        sc.next_seq()
        result = read_reply(s, timeout=10.0)
        if result and result[0] == 'reply':
            reply = result[1]
            add_len = struct.unpack_from('<I', reply, 4)[0]
            device_id = reply[1]
            min_kc = reply[10]
            max_kc = reply[11]
            present = struct.unpack_from('<H', reply, 12)[0]
            first_type = reply[14]
            n_types = reply[15]
            total_types = reply[16]
            first_key_sym = reply[17]
            total_syms = struct.unpack_from('<H', reply, 18)[0]
            n_key_syms = reply[20]

            print(f"  XKB GetMap: {len(reply)} bytes, add_len={add_len}")
            print(f"    device={device_id} keycodes={min_kc}-{max_kc} present=0x{present:04x}")
            print(f"    types: first={first_type} n={n_types} total={total_types}")
            print(f"    syms: first={first_key_sym} total={total_syms} nKeys={n_key_syms}")

            # Validate header size (should be 40 bytes)
            # total reply = 32 + add_len*4
            expected_total = 32 + add_len * 4
            if len(reply) != expected_total:
                print(f"    ERROR: reply is {len(reply)} bytes but expected {expected_total}!")

            # Validate body
            body = reply[40:]  # body starts after 40-byte header
            print(f"    body: {len(body)} bytes")

            # Parse types if present
            if present & 0x01:
                off = 0
                for t in range(n_types):
                    if off + 8 > len(body):
                        print(f"    ERROR: types data truncated at type {t}")
                        break
                    mods_mask = body[off]
                    mods_mods = body[off+1]
                    mods_vmods = struct.unpack_from('<H', body, off+2)[0]
                    num_levels = body[off+4]
                    n_map_entries = body[off+5]
                    has_preserve = body[off+6]
                    off += 8
                    print(f"    type {t}: mask=0x{mods_mask:02x} levels={num_levels} entries={n_map_entries} preserve={has_preserve}")
                    # Read map entries
                    for e in range(n_map_entries):
                        if off + 8 > len(body):
                            print(f"    ERROR: map entry truncated")
                            break
                        active = body[off]
                        entry_mask = body[off+1]
                        level = body[off+2]
                        entry_mods = body[off+3]
                        off += 8  # 4 bytes + vmods(2) + pad(2)
                        print(f"      entry: active={active} mask=0x{entry_mask:02x} level={level}")
                    # Read preserve entries
                    if has_preserve:
                        for e in range(n_map_entries):
                            off += 4  # preserve entry
                        print(f"      (has preserve data)")

                print(f"    types consumed {off} bytes")
                types_end = off
            else:
                types_end = 0

            # Parse syms
            if present & 0x02:
                off = types_end
                sym_count = 0
                for k in range(n_key_syms):
                    if off + 8 > len(body):
                        print(f"    ERROR: syms data truncated at key {k}")
                        break
                    kt0 = body[off]; kt1 = body[off+1]; kt2 = body[off+2]; kt3 = body[off+3]
                    group_info = body[off+4]
                    width = body[off+5]
                    n_syms_k = struct.unpack_from('<H', body, off+6)[0]
                    off += 8
                    # Read keysyms
                    for _ in range(n_syms_k):
                        if off + 4 > len(body):
                            print(f"    ERROR: keysym data truncated")
                            break
                        ks = struct.unpack_from('<I', body, off)[0]
                        off += 4
                    sym_count += n_syms_k

                    # First few and last
                    if k < 3 or k == n_key_syms - 1:
                        print(f"    key[{k}] kc={first_key_sym+k}: kt={kt0} groups={group_info&0xf} width={width} syms={n_syms_k}")

                print(f"    total syms parsed: {sym_count} (expected {total_syms})")
                if sym_count != total_syms:
                    print(f"    ERROR: sym count mismatch!")
                syms_end = off
            else:
                syms_end = types_end

            # Parse modmap
            if present & 0x04:
                off = syms_end
                n_mod_keys = reply[32]  # nModMapKeys
                print(f"    modmap: {n_mod_keys} keys starting at offset {off}")
                for m in range(n_mod_keys):
                    if off + 4 > len(body):
                        print(f"    ERROR: modmap truncated")
                        break
                    keycode = body[off]
                    mods = body[off+1]
                    off += 4
                    print(f"      keycode={keycode} mods=0x{mods:02x}")
        else:
            print(f"  XKB GetMap: FAILED")

    print()
    print("--- Phase 7: InternAtom (sample) ---")

    # Send a few InternAtom requests like Chrome does
    atoms_to_intern = [
        "WM_PROTOCOLS", "WM_DELETE_WINDOW", "WM_STATE",
        "_NET_WM_NAME", "_NET_WM_STATE", "_NET_WM_STATE_FULLSCREEN",
        "UTF8_STRING", "ATOM", "CARDINAL",
        "_NET_SUPPORTED",
    ]

    for name in atoms_to_intern:
        intern_atom(s, sc, name, only_if_exists=False)

    for name in atoms_to_intern:
        atom = read_intern_atom_reply(s)
        print(f"  InternAtom({name}): atom={atom}")

    print()
    print("--- Phase 8: XI2 SelectEvents ---")

    if "XInputExtension" in ext_info:
        opcode = ext_info["XInputExtension"][1]
        # XISelectEvents on root window (0x00000001)
        # deviceid=0 (XIAllDevices), mask_len=1 (4 bytes)
        # mask = XI_KeyPress | XI_KeyRelease | XI_ButtonPress | XI_ButtonRelease | XI_Motion | XI_Enter | XI_Leave | XI_FocusIn | XI_FocusOut
        xi2_mask = (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 10)
        data = struct.pack('<IH2x HH I',
            0x00000001,  # window = root
            1,           # num_masks
            0,           # deviceid = XIAllDevices
            1,           # mask_len = 1 word
            xi2_mask,    # mask
        )
        total = 4 + len(data)
        header = struct.pack('<BBH', opcode, 46, total // 4)
        s.sendall(header + data)
        sc.next_seq()
        print(f"  XISelectEvents: sent (no reply expected)")

    print()
    print("--- Phase 9: ListInputDevices ---")

    if "XInputExtension" in ext_info:
        opcode = ext_info["XInputExtension"][1]
        # ListInputDevices: sub-opcode 2, no data
        header = struct.pack('<BBH', opcode, 2, 1)
        s.sendall(header)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            num_devices = reply[8]
            add_len = struct.unpack_from('<I', reply, 4)[0]
            print(f"  ListInputDevices: {num_devices} devices, {len(reply)} bytes")

            # Parse devices
            body = reply[32:]
            off = 0
            for i in range(num_devices):
                if off + 8 > len(body):
                    print(f"    ERROR: device info truncated at device {i}")
                    break
                type_atom = struct.unpack_from('<I', body, off)[0]
                dev_id = body[off+4]
                num_classes = body[off+5]
                use_type = body[off+6]
                attached = body[off+7]
                off += 8

                use_names = {0:'IsXPointer', 1:'IsXKeyboard', 2:'IsXExtensionDevice', 3:'IsXExtensionPointer', 4:'IsXExtensionKeyboard'}
                print(f"    device {i}: id={dev_id} type_atom={type_atom} use={use_names.get(use_type, use_type)} classes={num_classes} attached={attached}")

                # Validate use_type consistency
                if dev_id == 4 and use_type != 3:  # Slave Pointer should be IsXExtensionPointer
                    print(f"    WARNING: Slave Pointer (id=4) has use_type={use_type}, expected 3 (IsXExtensionPointer)!")
                if dev_id == 5 and use_type != 4:  # Slave Keyboard should be IsXExtensionKeyboard
                    print(f"    WARNING: Slave Keyboard (id=5) has use_type={use_type}, expected 4 (IsXExtensionKeyboard)!")

            # Parse class infos
            print(f"    Class infos start at offset {off}:")
            classes_start = off
            for i in range(num_devices):
                # Re-read num_classes for this device
                dev_off = i * 8
                nc = body[dev_off + 5]
                for c in range(nc):
                    if off + 2 > len(body):
                        print(f"    ERROR: class info truncated")
                        break
                    cls = body[off]
                    length = body[off+1]
                    print(f"    device {i} class: type={cls} length={length}")
                    if cls == 0:  # KeyClass
                        min_kc = body[off+2]
                        max_kc = body[off+3]
                        num_keys = struct.unpack_from('<H', body, off+4)[0]
                        print(f"      keys: {min_kc}-{max_kc} ({num_keys} keys)")
                    off += length

            # Parse names
            print(f"    Names start at offset {off}:")
            for i in range(num_devices):
                if off >= len(body):
                    print(f"    ERROR: names truncated at device {i}")
                    break
                name_len = body[off]; off += 1
                if off + name_len > len(body):
                    print(f"    ERROR: name data truncated")
                    break
                name = body[off:off+name_len].decode('ascii', errors='replace')
                off += name_len
                print(f"    device {i} name: '{name}'")

            print(f"    Total body parsed: {off} bytes (body size: {len(body)})")
        else:
            print(f"  ListInputDevices: FAILED")

    print()
    print("--- Phase 10: XIQueryDevice ---")

    if "XInputExtension" in ext_info:
        opcode = ext_info["XInputExtension"][1]
        # XIQueryDevice: sub-opcode 48, deviceid=0 (XIAllDevices)
        data = struct.pack('<HH', 0, 0)  # deviceid=0, pad
        total = 4 + len(data)
        header = struct.pack('<BBH', opcode, 48, total // 4)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s, timeout=10.0)
        if result and result[0] == 'reply':
            reply = result[1]
            num_devices = struct.unpack_from('<H', reply, 8)[0]
            add_len = struct.unpack_from('<I', reply, 4)[0]
            print(f"  XIQueryDevice: {num_devices} devices, {len(reply)} bytes (add_len={add_len})")

            body = reply[32:]
            off = 0
            for i in range(num_devices):
                if off + 12 > len(body):
                    print(f"    ERROR: device info truncated at {i}")
                    break
                dev_id = struct.unpack_from('<H', body, off)[0]
                dev_type = struct.unpack_from('<H', body, off+2)[0]
                attachment = struct.unpack_from('<H', body, off+4)[0]
                num_classes = struct.unpack_from('<H', body, off+6)[0]
                name_len = struct.unpack_from('<H', body, off+8)[0]
                enabled = body[off+10]
                off += 12

                name_pad = (4 - (name_len % 4)) % 4
                name = body[off:off+name_len].decode('ascii', errors='replace')
                off += name_len + name_pad

                type_names = {1:'MasterPointer', 2:'MasterKeyboard', 3:'SlavePointer', 4:'SlaveKeyboard'}
                print(f"    device {dev_id}: type={type_names.get(dev_type, dev_type)} '{name}' classes={num_classes} attachment={attachment}")

                # Parse classes
                for c in range(num_classes):
                    if off + 6 > len(body):
                        print(f"      ERROR: class header truncated")
                        break
                    cls_type = struct.unpack_from('<H', body, off)[0]
                    cls_len_words = struct.unpack_from('<H', body, off+2)[0]
                    cls_source = struct.unpack_from('<H', body, off+4)[0]
                    cls_len_bytes = cls_len_words * 4

                    cls_names = {0:'Key', 1:'Button', 2:'Valuator', 3:'Scroll'}
                    print(f"      class {cls_names.get(cls_type, cls_type)}: {cls_len_words} words source={cls_source}")

                    if cls_type == 0:  # KeyClass
                        num_kc = struct.unpack_from('<H', body, off+6)[0]
                        print(f"        keycodes: {num_kc}")
                    elif cls_type == 1:  # ButtonClass
                        num_btns = struct.unpack_from('<H', body, off+6)[0]
                        print(f"        buttons: {num_btns}")
                    elif cls_type == 2:  # ValuatorClass
                        number = struct.unpack_from('<H', body, off+6)[0]
                        label = struct.unpack_from('<I', body, off+8)[0]
                        print(f"        valuator {number} label_atom={label}")

                    off += cls_len_bytes

            print(f"    Total body parsed: {off} bytes (body size: {len(body)})")
            if off != len(body):
                print(f"    WARNING: {len(body) - off} bytes remaining!")
        else:
            print(f"  XIQueryDevice: FAILED")

    print()
    print("--- Phase 11: RANDR Queries ---")

    if "RANDR" in ext_info:
        randr_opcode = ext_info["RANDR"][1]

        # GetOutputPrimary (sub-opcode 31)
        data = struct.pack('<I', 0x00000001)  # root window
        header = struct.pack('<BBH', randr_opcode, 31, 2)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s)
        if result and result[0] == 'reply':
            reply = result[1]
            primary = struct.unpack_from('<I', reply, 8)[0]
            print(f"  GetOutputPrimary: output=0x{primary:x}")
        else:
            print(f"  GetOutputPrimary: FAILED")

        # GetScreenResourcesCurrent (sub-opcode 25)
        data = struct.pack('<I', 0x00000001)  # root window
        header = struct.pack('<BBH', randr_opcode, 25, 2)
        s.sendall(header + data)
        sc.next_seq()
        result = read_reply(s, timeout=5.0)
        if result and result[0] == 'reply':
            reply = result[1]
            add_len = struct.unpack_from('<I', reply, 4)[0]
            body = reply[32:]
            timestamp = struct.unpack_from('<I', body, 0)[0]
            n_crtcs = struct.unpack_from('<H', body, 8)[0]
            n_outputs = struct.unpack_from('<H', body, 10)[0]
            n_modes = struct.unpack_from('<H', body, 12)[0]
            names_len = struct.unpack_from('<H', body, 14)[0]
            print(f"  GetScreenResourcesCurrent: {len(reply)} bytes, {n_crtcs} crtcs, {n_outputs} outputs, {n_modes} modes, namesLen={names_len}")

            # Parse CRTCs
            off = 24
            crtcs = []
            for i in range(n_crtcs):
                crtc = struct.unpack_from('<I', body, off)[0]
                crtcs.append(crtc)
                off += 4
            print(f"    CRTCs: {['0x%x' % c for c in crtcs]}")

            # Parse outputs
            outputs = []
            for i in range(n_outputs):
                output = struct.unpack_from('<I', body, off)[0]
                outputs.append(output)
                off += 4
            print(f"    Outputs: {['0x%x' % o for o in outputs]}")

            # Parse modes
            for i in range(n_modes):
                mode_id = struct.unpack_from('<I', body, off)[0]
                w = struct.unpack_from('<H', body, off+4)[0]
                h = struct.unpack_from('<H', body, off+6)[0]
                name_len = struct.unpack_from('<H', body, off+28)[0]
                off += 32
                print(f"    Mode 0x{mode_id:x}: {w}x{h}")

            # Parse mode names
            mode_names_data = body[off:off+names_len]
            print(f"    Mode names: {mode_names_data}")
        else:
            print(f"  GetScreenResourcesCurrent: FAILED")

        # GetOutputInfo (sub-opcode 14)
        if outputs:
            data = struct.pack('<II', outputs[0], 0)  # output, timestamp
            header = struct.pack('<BBH', randr_opcode, 14, 3)
            s.sendall(header + data)
            sc.next_seq()
            result = read_reply(s)
            if result and result[0] == 'reply':
                reply = result[1]
                connection = reply[20]
                num_crtcs = struct.unpack_from('<H', reply, 22)[0]
                num_modes = struct.unpack_from('<H', reply, 24)[0]
                name_len = struct.unpack_from('<H', reply, 30)[0]
                conn_names = {0:'Connected', 1:'Disconnected', 2:'Unknown'}
                print(f"  GetOutputInfo: {conn_names.get(connection, connection)} crtcs={num_crtcs} modes={num_modes} name_len={name_len}")
            else:
                print(f"  GetOutputInfo: FAILED")

        # GetCrtcInfo (sub-opcode 20)
        if crtcs:
            data = struct.pack('<II', crtcs[0], 0)  # crtc, timestamp
            header = struct.pack('<BBH', randr_opcode, 20, 3)
            s.sendall(header + data)
            sc.next_seq()
            result = read_reply(s)
            if result and result[0] == 'reply':
                reply = result[1]
                x = struct.unpack_from('<h', reply, 12)[0]
                y = struct.unpack_from('<h', reply, 14)[0]
                w = struct.unpack_from('<H', reply, 16)[0]
                h = struct.unpack_from('<H', reply, 18)[0]
                mode = struct.unpack_from('<I', reply, 20)[0]
                n_outputs = struct.unpack_from('<H', reply, 28)[0]
                n_possible = struct.unpack_from('<H', reply, 30)[0]
                print(f"  GetCrtcInfo: {x},{y} {w}x{h} mode=0x{mode:x} outputs={n_outputs} possible={n_possible}")
            else:
                print(f"  GetCrtcInfo: FAILED")

    print()
    print("--- Phase 12: RENDER QueryPictFormats ---")

    if "RENDER" in ext_info:
        opcode = ext_info["RENDER"][1]
        header = struct.pack('<BBH', opcode, 1, 1)
        s.sendall(header)
        sc.next_seq()
        result = read_reply(s, timeout=5.0)
        if result and result[0] == 'reply':
            reply = result[1]
            add_len = struct.unpack_from('<I', reply, 4)[0]
            num_formats = struct.unpack_from('<I', reply, 8)[0]
            num_screens = struct.unpack_from('<I', reply, 12)[0]
            num_subpixels = struct.unpack_from('<I', reply, 16)[0]
            print(f"  QueryPictFormats: {num_formats} formats, {num_screens} screens, {num_subpixels} subpixels, {len(reply)} bytes")

            body = reply[32:]
            off = 0
            for i in range(num_formats):
                if off + 28 > len(body):
                    print(f"    ERROR: format truncated at {i}")
                    break
                fmt_id = struct.unpack_from('<I', body, off)[0]
                fmt_type = body[off+4]
                fmt_depth = body[off+5]
                alpha_s = struct.unpack_from('<H', body, off+8)[0]
                alpha_m = struct.unpack_from('<H', body, off+10)[0]
                red_s = struct.unpack_from('<H', body, off+12)[0]
                red_m = struct.unpack_from('<H', body, off+14)[0]
                print(f"    format 0x{fmt_id:x}: type={fmt_type} depth={fmt_depth} alpha={alpha_s}/{alpha_m} red={red_s}/{red_m}")
                off += 28
        else:
            print(f"  QueryPictFormats: FAILED")

    print()
    print("=== Test Complete ===")
    s.close()


if __name__ == '__main__':
    test_chrome_sequence()
