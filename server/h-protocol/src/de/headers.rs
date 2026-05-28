use std::net::{Ipv4Addr, Ipv6Addr};

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Link-type constants
// ---------------------------------------------------------------------------
pub const LINKTYPE_NULL: u32 = 0;
pub const LINKTYPE_ETHERNET: u32 = 1;
pub const LINKTYPE_RAW: u32 = 101;
pub const LINKTYPE_LINUX_SLL: u32 = 113;
pub const LINKTYPE_LINUX_SLL2: u32 = 276;

// ---------------------------------------------------------------------------
// EtherType constants
// ---------------------------------------------------------------------------
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;
pub const ETHERTYPE_VLAN: u16 = 0x8100;
pub const ETHERTYPE_QINQ: u16 = 0x88A8;
pub const ETHERTYPE_MPLS: u16 = 0x8847;

// ---------------------------------------------------------------------------
// IP protocol constants
// ---------------------------------------------------------------------------
pub const IP_PROTO_TCP: u8 = 6;

// ---------------------------------------------------------------------------
// Address-family constants
// ---------------------------------------------------------------------------
pub const AF_INET: u32 = 2;
pub const AF_INET6_BSD: u32 = 30;
pub const AF_INET6_LINUX: u32 = 10;

// ---------------------------------------------------------------------------
// Ethernet (14 bytes)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct EthernetHeader {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ether_type: [u8; 2],
}

impl EthernetHeader {
    #[inline]
    pub fn ether_type(&self) -> u16 {
        u16::from_be_bytes(self.ether_type)
    }
}

// ---------------------------------------------------------------------------
// VLAN (4 bytes)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct VlanHeader {
    pub tci: [u8; 2],
    pub ether_type: [u8; 2],
}

impl VlanHeader {
    #[inline]
    pub fn ether_type(&self) -> u16 {
        u16::from_be_bytes(self.ether_type)
    }
}

// ---------------------------------------------------------------------------
// MPLS shim (4 bytes): 20-bit label, 3-bit TC, 1-bit S (bottom-of-stack), 8-bit TTL.
// Network byte order. We only need the S bit to know when to stop unwinding
// the label stack.
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct MplsHeader {
    pub bytes: [u8; 4],
}

impl MplsHeader {
    /// True when this label is the bottom-of-stack (S bit set).
    ///
    /// In the 32-bit label entry, the S bit is the LSB of the third byte.
    #[inline]
    pub fn bottom_of_stack(&self) -> bool {
        self.bytes[2] & 0x01 != 0
    }
}

// ---------------------------------------------------------------------------
// Linux cooked capture v1 / SLL (16 bytes)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LinuxSllHeader {
    pub packet_type: [u8; 2],
    pub arphrd_type: [u8; 2],
    pub addr_len: [u8; 2],
    pub addr: [u8; 8],
    pub protocol: [u8; 2],
}

impl LinuxSllHeader {
    #[inline]
    pub fn protocol(&self) -> u16 {
        u16::from_be_bytes(self.protocol)
    }
}

// ---------------------------------------------------------------------------
// Linux cooked capture v2 / SLL2 (20 bytes)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct LinuxSll2Header {
    pub protocol: [u8; 2],
    pub _reserved: [u8; 2],
    pub iface_index: [u8; 4],
    pub arphrd_type: [u8; 2],
    pub packet_type: u8,
    pub addr_len: u8,
    pub addr: [u8; 8],
}

impl LinuxSll2Header {
    #[inline]
    pub fn protocol(&self) -> u16 {
        u16::from_be_bytes(self.protocol)
    }
}

// ---------------------------------------------------------------------------
// BSD loopback / NULL (4 bytes) — address family in host byte order
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct NullHeader {
    pub af_family: [u8; 4],
}

impl NullHeader {
    #[inline]
    pub fn af_family(&self) -> u32 {
        u32::from_ne_bytes(self.af_family)
    }
}

// ---------------------------------------------------------------------------
// IPv4 (20 bytes minimum)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Ipv4Header {
    pub ver_ihl: u8,
    pub tos: u8,
    pub total_length: [u8; 2],
    pub identification: [u8; 2],
    pub flags_frag: [u8; 2],
    pub ttl: u8,
    pub protocol: u8,
    pub checksum: [u8; 2],
    pub src: [u8; 4],
    pub dst: [u8; 4],
}

impl Ipv4Header {
    /// Internet header length in bytes (IHL field × 4).
    #[inline]
    pub fn ihl(&self) -> usize {
        ((self.ver_ihl & 0x0F) as usize) * 4
    }

    #[inline]
    pub fn total_length(&self) -> u16 {
        u16::from_be_bytes(self.total_length)
    }

    #[inline]
    pub fn protocol(&self) -> u8 {
        self.protocol
    }

    #[inline]
    pub fn src_ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.src)
    }

    #[inline]
    pub fn dst_ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.dst)
    }
}

// ---------------------------------------------------------------------------
// IPv6 (40 bytes)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct Ipv6Header {
    pub ver_tc_fl: [u8; 4],
    pub payload_length: [u8; 2],
    pub next_header: u8,
    pub hop_limit: u8,
    pub src: [u8; 16],
    pub dst: [u8; 16],
}

impl Ipv6Header {
    #[inline]
    pub fn payload_length(&self) -> u16 {
        u16::from_be_bytes(self.payload_length)
    }

    #[inline]
    pub fn next_header(&self) -> u8 {
        self.next_header
    }

    #[inline]
    pub fn src_ip(&self) -> Ipv6Addr {
        Ipv6Addr::from(self.src)
    }

    #[inline]
    pub fn dst_ip(&self) -> Ipv6Addr {
        Ipv6Addr::from(self.dst)
    }
}

// ---------------------------------------------------------------------------
// TCP (20 bytes minimum)
// ---------------------------------------------------------------------------
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct TcpHeader {
    pub src_port: [u8; 2],
    pub dst_port: [u8; 2],
    pub seq: [u8; 4],
    pub ack: [u8; 4],
    pub data_offset_flags: [u8; 2],
    pub window: [u8; 2],
    pub checksum: [u8; 2],
    pub urgent: [u8; 2],
}

impl TcpHeader {
    #[inline]
    pub fn src_port(&self) -> u16 {
        u16::from_be_bytes(self.src_port)
    }

    #[inline]
    pub fn dst_port(&self) -> u16 {
        u16::from_be_bytes(self.dst_port)
    }

    #[inline]
    pub fn seq(&self) -> u32 {
        u32::from_be_bytes(self.seq)
    }

    #[inline]
    pub fn ack(&self) -> u32 {
        u32::from_be_bytes(self.ack)
    }

    /// Data offset (TCP header length) in bytes.
    #[inline]
    pub fn data_offset(&self) -> usize {
        ((self.data_offset_flags[0] >> 4) as usize) * 4
    }

    /// Flags byte (lower byte of the data_offset_flags field).
    #[inline]
    pub fn flags(&self) -> u8 {
        self.data_offset_flags[1]
    }
}
