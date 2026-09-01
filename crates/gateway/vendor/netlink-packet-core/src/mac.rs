// SPDX-License-Identifier: MIT

//! Ethernet protocol numbers (`ETH_P_*`), as defined by the Linux kernel
//! in `include/uapi/linux/if_ether.h`.

macro_rules! ethernet_protocol {
    ($(($variant:ident, $const:ident, $value:expr, $display:literal)),* $(,)?) => {
        $(
            const $const: u16 = $value;
        )*

        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        #[non_exhaustive]
        pub enum EthernetProtocol {
            $($variant,)*
            Other(u16),
        }

        impl EthernetProtocol {
            pub fn value(&self) -> u16 {
                match self {
                    $(Self::$variant => $const,)*
                    Self::Other(v) => *v,
                }
            }
        }

        impl From<u16> for EthernetProtocol {
            fn from(d: u16) -> Self {
                match d {
                    $($const => Self::$variant,)*
                    _ => Self::Other(d),
                }
            }
        }

        impl std::fmt::Display for EthernetProtocol {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    $(Self::$variant => write!(f, $display),)*
                    Self::Other(v) => write!(f, "{v:#x}"),
                }
            }
        }
    };
}

ethernet_protocol! {
    // Ethernet II ethertypes
    (Loop, ETH_P_LOOP, 0x0060, "loop"),
    (Pup, ETH_P_PUP, 0x0200, "pup"),
    (Pupat, ETH_P_PUPAT, 0x0201, "pupat"),
    (Tsn, ETH_P_TSN, 0x22F0, "tsn"),
    (Erspan2, ETH_P_ERSPAN2, 0x22EB, "erspan2"),
    (Ip, ETH_P_IP, 0x0800, "ip"),
    (X25, ETH_P_X25, 0x0805, "x25"),
    (Arp, ETH_P_ARP, 0x0806, "arp"),
    (Bpq, ETH_P_BPQ, 0x08FF, "bpq"),
    (IeeePup, ETH_P_IEEEPUP, 0x0a00, "ieeepup"),
    (IeeePupat, ETH_P_IEEEPUPAT, 0x0a01, "ieeepupat"),
    (Batman, ETH_P_BATMAN, 0x4305, "batman"),
    (Dec, ETH_P_DEC, 0x6000, "dec"),
    (DnaDl, ETH_P_DNA_DL, 0x6001, "dna_dl"),
    (DnaRc, ETH_P_DNA_RC, 0x6002, "dna_rc"),
    (DnaRt, ETH_P_DNA_RT, 0x6003, "dna_rt"),
    (Lat, ETH_P_LAT, 0x6004, "lat"),
    (Diag, ETH_P_DIAG, 0x6005, "diag"),
    (Cust, ETH_P_CUST, 0x6006, "cust"),
    (Sca, ETH_P_SCA, 0x6007, "sca"),
    (Teb, ETH_P_TEB, 0x6558, "teb"),
    (Rarp, ETH_P_RARP, 0x8035, "rarp"),
    (Atalk, ETH_P_ATALK, 0x809B, "atalk"),
    (Aarp, ETH_P_AARP, 0x80F3, "aarp"),
    (E8021Q, ETH_P_8021Q, 0x8100, "8021q"),
    (Erspan, ETH_P_ERSPAN, 0x88BE, "erspan"),
    (Ipx, ETH_P_IPX, 0x8137, "ipx"),
    (Ipv6, ETH_P_IPV6, 0x86DD, "ipv6"),
    (Pause, ETH_P_PAUSE, 0x8808, "pause"),
    (Slow, ETH_P_SLOW, 0x8809, "slow"),
    (Wccp, ETH_P_WCCP, 0x883E, "wccp"),
    (MplsUc, ETH_P_MPLS_UC, 0x8847, "mpls_uc"),
    (MplsMc, ETH_P_MPLS_MC, 0x8848, "mpls_mc"),
    (Atmmpoa, ETH_P_ATMMPOA, 0x884c, "atmmpoa"),
    (PppDisc, ETH_P_PPP_DISC, 0x8863, "ppp_disc"),
    (PppSes, ETH_P_PPP_SES, 0x8864, "ppp_ses"),
    (LinkCtl, ETH_P_LINK_CTL, 0x886c, "link_ctl"),
    (Atmfate, ETH_P_ATMFATE, 0x8884, "atmfate"),
    (Pae, ETH_P_PAE, 0x888E, "pae"),
    (Profinet, ETH_P_PROFINET, 0x8892, "profinet"),
    (Realtek, ETH_P_REALTEK, 0x8899, "realtek"),
    (Aoe, ETH_P_AOE, 0x88A2, "aoe"),
    (Ethercat, ETH_P_ETHERCAT, 0x88A4, "ethercat"),
    (E8021Ad, ETH_P_8021AD, 0x88A8, "8021ad"),
    (E802Ex1, ETH_P_802_EX1, 0x88B5, "802_ex1"),
    (MxlGsw, ETH_P_MXLGSW, 0x88C3, "mxl_gsw"),
    (Preauth, ETH_P_PREAUTH, 0x88C7, "preauth"),
    (Tipc, ETH_P_TIPC, 0x88CA, "tipc"),
    (Lldp, ETH_P_LLDP, 0x88CC, "lldp"),
    (Mrp, ETH_P_MRP, 0x88E3, "mrp"),
    (Macsec, ETH_P_MACSEC, 0x88E5, "macsec"),
    (E8021Ah, ETH_P_8021AH, 0x88E7, "8021ah"),
    (Mvrp, ETH_P_MVRP, 0x88F5, "mvrp"),
    (E1588, ETH_P_1588, 0x88F7, "1588"),
    (Ncsi, ETH_P_NCSI, 0x88F8, "ncsi"),
    (Prp, ETH_P_PRP, 0x88FB, "prp"),
    (Cfm, ETH_P_CFM, 0x8902, "cfm"),
    (Fcoe, ETH_P_FCOE, 0x8906, "fcoe"),
    (Iboe, ETH_P_IBOE, 0x8915, "iboe"),
    (Tdls, ETH_P_TDLS, 0x890D, "tdls"),
    (Fip, ETH_P_FIP, 0x8914, "fip"),
    (E80221, ETH_P_80221, 0x8917, "80221"),
    (Hsr, ETH_P_HSR, 0x892F, "hsr"),
    (Nsh, ETH_P_NSH, 0x894F, "nsh"),
    (Loopback, ETH_P_LOOPBACK, 0x9000, "loopback"),
    (Qinq1, ETH_P_QINQ1, 0x9100, "qinq1"),
    (Qinq2, ETH_P_QINQ2, 0x9200, "qinq2"),
    (Qinq3, ETH_P_QINQ3, 0x9300, "qinq3"),
    (Yt921x, ETH_P_YT921X, 0x9988, "yt921x"),
    (Edsa, ETH_P_EDSA, 0xDADA, "edsa"),
    (Dsa8021Q, ETH_P_DSA_8021Q, 0xDADB, "dsa_8021q"),
    (DsaA5psw, ETH_P_DSA_A5PSW, 0xE001, "dsa_a5psw"),
    (Ife, ETH_P_IFE, 0xED3E, "ife"),
    (AfIucv, ETH_P_AF_IUCV, 0xFBFB, "af_iucv"),

    // Non-DIX types
    (E802_3, ETH_P_802_3, 0x0001, "802_3"),
    (Ax25, ETH_P_AX25, 0x0002, "ax25"),
    (All, ETH_P_ALL, 0x0003, "all"),
    (E802_2, ETH_P_802_2, 0x0004, "802_2"),
    (Snap, ETH_P_SNAP, 0x0005, "snap"),
    (Ddcmp, ETH_P_DDCMP, 0x0006, "ddcmp"),
    (WanPpp, ETH_P_WAN_PPP, 0x0007, "wan_ppp"),
    (PppMp, ETH_P_PPP_MP, 0x0008, "ppp_mp"),
    (Localtalk, ETH_P_LOCALTALK, 0x0009, "localtalk"),
    (Can, ETH_P_CAN, 0x000C, "can"),
    (Canfd, ETH_P_CANFD, 0x000D, "canfd"),
    (Canxl, ETH_P_CANXL, 0x000E, "canxl"),
    (Ppptalk, ETH_P_PPPTALK, 0x0010, "ppptalk"),
    (Tr802_2, ETH_P_TR_802_2, 0x0011, "tr_802_2"),
    (Mobitex, ETH_P_MOBITEX, 0x0015, "mobitex"),
    (Control, ETH_P_CONTROL, 0x0016, "control"),
    (Irda, ETH_P_IRDA, 0x0017, "irda"),
    (Econet, ETH_P_ECONET, 0x0018, "econet"),
    (Hdlc, ETH_P_HDLC, 0x0019, "hdlc"),
    (Arcnet, ETH_P_ARCNET, 0x001A, "arcnet"),
    (Dsa, ETH_P_DSA, 0x001B, "dsa"),
    (Trailer, ETH_P_TRAILER, 0x001C, "trailer"),
    (Phonet, ETH_P_PHONET, 0x00F5, "phonet"),
    (Ieee802154, ETH_P_IEEE802154, 0x00F6, "ieee802154"),
    (Caif, ETH_P_CAIF, 0x00F7, "caif"),
    (Xdsa, ETH_P_XDSA, 0x00F8, "xdsa"),
    (Map, ETH_P_MAP, 0x00F9, "map"),
    (Mctp, ETH_P_MCTP, 0x00FA, "mctp"),
}

impl From<EthernetProtocol> for u16 {
    fn from(v: EthernetProtocol) -> u16 {
        v.value()
    }
}
