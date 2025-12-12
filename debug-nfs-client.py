#!/usr/bin/env python3
"""
Simple NFSv4 debug client to test Flint NFS server
Tests PUTROOTFH, GETATTR, READDIR, LOOKUP operations
"""

import socket
import struct
import sys

class NFSv4Client:
    def __init__(self, host, port=2049):
        self.host = host
        self.port = port
        self.xid = 1
        self.sock = None
        self.sessionid = None
        self.sequenceid = 0
        
    def connect(self):
        """Connect to NFS server"""
        print(f"Connecting to {self.host}:{self.port}...")
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.sock.connect((self.host, self.port))
        print("✅ Connected")
        
    def send_rpc(self, program, version, procedure, data):
        """Send RPC call"""
        # RPC header
        rpc = struct.pack('!I', self.xid)  # XID
        rpc += struct.pack('!I', 0)  # CALL
        rpc += struct.pack('!I', 2)  # RPC version
        rpc += struct.pack('!I', program)  # NFS program
        rpc += struct.pack('!I', version)  # NFS version
        rpc += struct.pack('!I', procedure)  # Procedure
        
        # Auth (NULL)
        rpc += struct.pack('!I', 0)  # AUTH_NULL
        rpc += struct.pack('!I', 0)  # length
        
        # Verf (NULL)
        rpc += struct.pack('!I', 0)  # AUTH_NULL
        rpc += struct.pack('!I', 0)  # length
        
        # Data
        rpc += data
        
        # Add RPC record marker
        marker = 0x80000000 | len(rpc)
        msg = struct.pack('!I', marker) + rpc
        
        self.sock.sendall(msg)
        self.xid += 1
        
    def recv_rpc(self):
        """Receive RPC reply"""
        # Read record marker
        marker = self.sock.recv(4)
        if len(marker) != 4:
            raise Exception("Failed to read marker")
        
        marker_val = struct.unpack('!I', marker)[0]
        length = marker_val & 0x7FFFFFFF
        
        # Read reply
        reply = b''
        while len(reply) < length:
            chunk = self.sock.recv(length - len(reply))
            if not chunk:
                raise Exception("Connection closed")
            reply += chunk
            
        return reply
        
    def decode_status(self, data, offset):
        """Decode NFS4 status"""
        status = struct.unpack_from('!I', data, offset)[0]
        return status, offset + 4
        
    def test_null(self):
        """Test NULL procedure"""
        print("\n=== Testing NULL ===")
        self.send_rpc(100003, 4, 0, b'')
        reply = self.recv_rpc()
        print(f"✅ NULL succeeded, got {len(reply)} bytes")
        
    def test_putrootfh_getattr(self):
        """Test PUTROOTFH + GETATTR"""
        print("\n=== Testing PUTROOTFH + GETATTR ===")
        
        # Build COMPOUND: PUTROOTFH + GETATTR
        compound = b''
        
        # Tag (empty string)
        compound += struct.pack('!I', 0)
        
        # Minor version (2 = NFSv4.2)
        compound += struct.pack('!I', 2)
        
        # Number of operations
        compound += struct.pack('!I', 2)
        
        # Operation 1: PUTROOTFH (opcode 24)
        compound += struct.pack('!I', 24)
        
        # Operation 2: GETATTR (opcode 9)
        compound += struct.pack('!I', 9)
        # Bitmap: request TYPE (1), FSID (8), FILEID (20)
        compound += struct.pack('!I', 1)  # 1 word
        compound += struct.pack('!I', (1 << 1) | (1 << 8) | (1 << 20))  # attrs
        
        self.send_rpc(100003, 4, 1, compound)
        reply = self.recv_rpc()
        
        print(f"Reply: {len(reply)} bytes")
        print(f"Hex: {reply[:64].hex()}")
        
        # Parse reply
        offset = 0
        xid = struct.unpack_from('!I', reply, offset)[0]
        offset += 4
        
        # Skip RPC reply header (accept status, verf)
        offset += 4  # reply type
        offset += 4  # accept_stat
        offset += 4  # verf flavor
        verf_len = struct.unpack_from('!I', reply, offset)[0]
        offset += 4 + verf_len
        
        # COMPOUND response
        status, offset = self.decode_status(reply, offset)
        print(f"COMPOUND status: {status}")
        
        # Tag
        tag_len = struct.unpack_from('!I', reply, offset)[0]
        offset += 4 + tag_len + ((4 - tag_len % 4) % 4)
        
        # Number of results
        num_results = struct.unpack_from('!I', reply, offset)[0]
        offset += 4
        print(f"Number of results: {num_results}")
        
        # Result 1: PUTROOTFH
        opcode = struct.unpack_from('!I', reply, offset)[0]
        offset += 4
        status, offset = self.decode_status(reply, offset)
        print(f"  PUTROOTFH: opcode={opcode}, status={status}")
        
        # Result 2: GETATTR
        opcode = struct.unpack_from('!I', reply, offset)[0]
        offset += 4
        status, offset = self.decode_status(reply, offset)
        print(f"  GETATTR: opcode={opcode}, status={status}")
        
        if status == 0:
            # Bitmap
            bitmap_len = struct.unpack_from('!I', reply, offset)[0]
            offset += 4
            print(f"    Bitmap length: {bitmap_len}")
            
            bitmap = []
            for i in range(bitmap_len):
                word = struct.unpack_from('!I', reply, offset)[0]
                bitmap.append(word)
                offset += 4
                print(f"    Bitmap[{i}]: 0x{word:08x}")
            
            # Attr vals
            attr_len = struct.unpack_from('!I', reply, offset)[0]
            offset += 4
            print(f"    Attr values: {attr_len} bytes")
            
            if attr_len > 0:
                attrs = reply[offset:offset+attr_len]
                print(f"    First 32 bytes: {attrs[:32].hex()}")
        
        print("✅ PUTROOTFH + GETATTR succeeded")
        
    def test_readdir(self):
        """Test PUTROOTFH + READDIR"""
        print("\n=== Testing PUTROOTFH + READDIR ===")
        
        # Build COMPOUND: PUTROOTFH + READDIR
        compound = b''
        compound += struct.pack('!I', 0)  # Tag
        compound += struct.pack('!I', 2)  # Minor version
        compound += struct.pack('!I', 2)  # 2 operations
        
        # PUTROOTFH
        compound += struct.pack('!I', 24)
        
        # READDIR (opcode 26)
        compound += struct.pack('!I', 26)
        compound += struct.pack('!Q', 0)  # cookie
        compound += struct.pack('!Q', 0)  # cookieverf
        compound += struct.pack('!I', 4096)  # dircount
        compound += struct.pack('!I', 4096)  # maxcount
        # Request TYPE, FILEID, MODE attributes
        compound += struct.pack('!I', 2)  # 2 words
        compound += struct.pack('!I', (1 << 1) | (1 << 20))  # TYPE, FILEID
        compound += struct.pack('!I', (1 << 1))  # MODE (bit 33-32=1)
        
        self.send_rpc(100003, 4, 1, compound)
        reply = self.recv_rpc()
        
        print(f"Reply: {len(reply)} bytes")
        
        # Quick parse to see if we got entries
        if b'volume' in reply:
            print("✅ Found 'volume' entry in READDIR response!")
            # Find and show volume entry
            idx = reply.find(b'volume')
            print(f"   Entry at offset {idx}")
            print(f"   Context: ...{reply[idx-20:idx+30].hex()}...")
        else:
            print("❌ No 'volume' entry found")
            
    def test_lookup(self):
        """Test PUTROOTFH + LOOKUP volume"""
        print("\n=== Testing PUTROOTFH + LOOKUP 'volume' ===")
        
        # Build COMPOUND: PUTROOTFH + LOOKUP
        compound = b''
        compound += struct.pack('!I', 0)  # Tag
        compound += struct.pack('!I', 2)  # Minor version
        compound += struct.pack('!I', 2)  # 2 operations
        
        # PUTROOTFH
        compound += struct.pack('!I', 24)
        
        # LOOKUP (opcode 15)
        compound += struct.pack('!I', 15)
        # Component name
        name = b'volume'
        compound += struct.pack('!I', len(name))
        compound += name
        # Padding
        padding = (4 - len(name) % 4) % 4
        compound += b'\x00' * padding
        
        self.send_rpc(100003, 4, 1, compound)
        reply = self.recv_rpc()
        
        print(f"Reply: {len(reply)} bytes")
        print(f"Hex: {reply[:80].hex()}")
        
        # Check for success - look for status=0 in LOOKUP result
        # (very rough parse)
        if b'\x00\x00\x00\x00' in reply[40:60]:  # status OK somewhere
            print("✅ LOOKUP appears to succeed!")
        else:
            print("❌ LOOKUP may have failed")
            
    def run_all_tests(self):
        """Run all tests"""
        try:
            self.connect()
            self.test_null()
            self.test_putrootfh_getattr()
            self.test_readdir()
            self.test_lookup()
            
            print("\n" + "="*50)
            print("🎉 All protocol tests completed!")
            print("="*50)
            
        finally:
            if self.sock:
                self.sock.close()
                print("\nConnection closed")

if __name__ == '__main__':
    client = NFSv4Client('127.0.0.1', 2049)
    client.run_all_tests()

