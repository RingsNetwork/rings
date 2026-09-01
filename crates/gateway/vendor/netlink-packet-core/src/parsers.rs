// SPDX-License-Identifier: MIT

use std::{
    mem::size_of,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use paste::paste;

use crate::DecodeError;

pub fn parse_mac(payload: &[u8]) -> Result<[u8; 6], DecodeError> {
    if payload.len() != 6 {
        return Err(DecodeError::invalid_mac_address(payload.len()));
    }
    let mut address: [u8; 6] = [0; 6];
    for (i, byte) in payload.iter().enumerate() {
        address[i] = *byte;
    }
    Ok(address)
}

pub fn parse_ipv6(payload: &[u8]) -> Result<[u8; 16], DecodeError> {
    if payload.len() != 16 {
        return Err(DecodeError::invalid_ip_address(payload.len()));
    }
    let mut address: [u8; 16] = [0; 16];
    for (i, byte) in payload.iter().enumerate() {
        address[i] = *byte;
    }
    Ok(address)
}

pub fn parse_ip(payload: &[u8]) -> Result<IpAddr, DecodeError> {
    match payload.len() {
        4 => Ok(
            Ipv4Addr::new(payload[0], payload[1], payload[2], payload[3])
                .into(),
        ),
        16 => Ok(Ipv6Addr::from([
            payload[0],
            payload[1],
            payload[2],
            payload[3],
            payload[4],
            payload[5],
            payload[6],
            payload[7],
            payload[8],
            payload[9],
            payload[10],
            payload[11],
            payload[12],
            payload[13],
            payload[14],
            payload[15],
        ])
        .into()),
        other => Err(DecodeError::invalid_ip_address(other)),
    }
}

pub fn parse_string(payload: &[u8]) -> Result<String, DecodeError> {
    if payload.is_empty() {
        return Ok(String::new());
    }
    // iproute2 is a bit inconsistent with null-terminated strings.
    let slice = if payload[payload.len() - 1] == 0 {
        &payload[..payload.len() - 1]
    } else {
        &payload[..payload.len()]
    };
    let s = String::from_utf8(slice.to_vec())?;
    Ok(s)
}

pub fn parse_u8(payload: &[u8]) -> Result<u8, DecodeError> {
    if payload.len() != 1 {
        return Err(DecodeError::invalid_number(1, payload.len()));
    }
    Ok(payload[0])
}

pub fn parse_i8(payload: &[u8]) -> Result<i8, DecodeError> {
    if payload.len() != 1 {
        return Err(DecodeError::invalid_number(1, payload.len()));
    }
    Ok(payload[0] as i8)
}

macro_rules! gen_int_parser {
    ( $($data_type:ty,)+ ) => {
        $(

            paste! {
                pub fn [<parse_ $data_type >](
                    payload: &[u8]
                ) -> Result<$data_type, DecodeError> {
                    if payload.len() != size_of::<$data_type>() {
                        return Err(DecodeError::invalid_number(
                            size_of::<$data_type>(),
                            payload.len(),
                        ));
                    }
                    let mut data = [0u8; size_of::<$data_type>()];
                    data.copy_from_slice(payload);
                    Ok(<$data_type>::from_ne_bytes(data))
                }

                pub fn [<parse_ $data_type _be>](
                    payload: &[u8]
                ) -> Result<$data_type, DecodeError> {
                    if payload.len() != size_of::<$data_type>() {
                        return Err(DecodeError::invalid_number(
                            size_of::<$data_type>(),
                            payload.len(),
                        ));
                    }
                    let mut data = [0u8; size_of::<$data_type>()];
                    data.copy_from_slice(payload);
                    Ok(<$data_type>::from_be_bytes(data))
                }

                pub fn [<parse_ $data_type _le>](
                    payload: &[u8]
                ) -> Result<$data_type, DecodeError> {
                    if payload.len() != size_of::<$data_type>() {
                        return Err(DecodeError::invalid_number(
                            size_of::<$data_type>(),
                            payload.len(),
                        ));
                    }
                    let mut data = [0u8; size_of::<$data_type>()];
                    data.copy_from_slice(payload);
                    Ok(<$data_type>::from_le_bytes(data))
                }

                pub fn [<emit_ $data_type >](
                    buf: &mut [u8],
                    value: $data_type,
                ) -> Result<(), DecodeError> {
                    if buf.len() < size_of::<$data_type>() {
                        return Err(DecodeError::buffer_too_small(
                            buf.len(),
                            size_of::<$data_type>(),
                        ));
                    }
                    buf[..size_of::<$data_type>()].copy_from_slice(
                        &value.to_ne_bytes()
                    );
                    Ok(())
                }

                pub fn [<emit_ $data_type _le>](
                    buf: &mut [u8],
                    value: $data_type,
                ) -> Result<(), DecodeError> {
                    if buf.len() < size_of::<$data_type>() {
                        return Err(DecodeError::buffer_too_small(
                            buf.len(),
                            size_of::<$data_type>(),
                        ));
                    }
                    buf[..size_of::<$data_type>()].copy_from_slice(
                        &value.to_le_bytes()
                    );
                    Ok(())
                }

                pub fn [<emit_ $data_type _be>](
                    buf: &mut [u8],
                    value: $data_type,
                ) -> Result<(), DecodeError> {
                    if buf.len() < size_of::<$data_type>() {
                        return Err(DecodeError::buffer_too_small(
                            buf.len(),
                            size_of::<$data_type>(),
                        ));
                    }
                    buf[..size_of::<$data_type>()].copy_from_slice(
                        &value.to_be_bytes()
                    );
                    Ok(())
                }
            }
        )+
    }
}

gen_int_parser!(u16, u32, u64, u128, i16, i32, i64, i128,);
