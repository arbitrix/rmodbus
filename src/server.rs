#[path = "context.rs"]
pub mod context;

/// Standard Modbus frame
///
/// As max length of Modbus frame + headers is always 256 bytes or less, the frame is a fixed [u8;
/// 256] array.
pub type ModbusFrame = [u8; 256];

/// Modbus protocol selection for frame processing
///
/// * for **TcpUdp**, Modbus TCP headers are parsed / added to replies
/// * for **Rtu**, frame checksums are verified / added to repies
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub enum ModbusProto {
    Rtu,
    TcpUdp,
}

fn calc_rtu_crc(frame: &[u8], data_length: u8) -> u16 {
    let mut crc: u16 = 0xffff;
    for pos in 0..data_length as usize {
        crc = crc ^ frame[pos] as u16;
        for _ in (0..8).rev() {
            if (crc & 0x0001) != 0 {
                crc = crc >> 1;
                crc = crc ^ 0xA001;
            } else {
                crc = crc >> 1;
            }
        }
    }
    return crc;
}

/// Process Modbus frame
///
/// Simple example of Modbus/UDP blocking server:
///
/// ```
///
///use std::net::UdpSocket;
///
///use rmodbus::server::{ModbusFrame, ModbusProto, process_frame};
///
///pub fn udpserver(unit: u8, listen: &str) {
///    let socket = UdpSocket::bind(listen).unwrap();
///    loop {
///        // init frame buffer
///        let mut buf: ModbusFrame = [0; 256];
///        let (_amt, src) = socket.recv_from(&mut buf).unwrap();
///        // Send frame for processing - modify context for write frames and get response
///        let response: Vec<u8> = match process_frame(unit, &buf, ModbusProto::TcpUdp) {
///            Some(v) => v,
///            None => {
///                // continue loop (or exit function) if there's nothing to send as the reply
///                continue;
///            }
///        };
///        socket.send_to(response.as_slice(), &src).unwrap();
///    }
///}
/// ```
///
/// There are also [examples of TCP and
/// RTU](https://github.com/alttch/rmodbus/tree/master/examples/example-server/src)
///
/// The function returns None in cases:
///
/// * **incorrect frame header**: the frame header is absolutely incorrect and there's no way to
///     form a valid Modbus error reply
///
/// * **not my frame**: the specified unit id doesn't match unit id in Modbus frame
///
/// * **broadcast request**: when broadcasts are processed, apps shouldn't reply anything back
///
pub fn process_frame(unit_id: u8, frame: &ModbusFrame, proto: ModbusProto) -> Option<Vec<u8>> {
    let start_frame: usize;
    let mut response: Vec<u8> = Vec::new();
    if proto == ModbusProto::TcpUdp {
        //let tr_id = u16::from_be_bytes([frame[0], frame[1]]);
        let proto_id = u16::from_be_bytes([frame[2], frame[3]]);
        let length = u16::from_be_bytes([frame[4], frame[5]]);
        if proto_id != 0 || length < 6 {
            return None;
        }
        start_frame = 6;
    } else {
        start_frame = 0;
    }
    let unit = frame[start_frame];
    let broadcast = unit == 0 || unit == 255; // some clients send broadcast to 0xff
    if !broadcast && unit != unit_id {
        return None;
    }
    if !broadcast && proto == ModbusProto::TcpUdp {
        response.extend_from_slice(&frame[0..4]); // copy 4 bytes: tr id and proto
    }
    let func = frame[start_frame + 1];
    macro_rules! check_frame_crc {
        ($len:expr) => {
            proto == ModbusProto::TcpUdp
                || calc_rtu_crc(frame, $len)
                    == u16::from_le_bytes([frame[$len as usize], frame[$len as usize + 1]]);
        };
    }
    macro_rules! response_error {
        ($err:expr) => {
            match proto {
                ModbusProto::TcpUdp => {
                    response.extend_from_slice(&[0, 3, frame[7], frame[8] + 0x80, $err])
                }
                ModbusProto::Rtu => response.extend_from_slice(&[frame[0], frame[1] + 0x80, $err]),
            }
        };
    }
    macro_rules! response_set_data_len {
        ($len:expr) => {
            if proto == ModbusProto::TcpUdp {
                response.extend_from_slice(&($len as u16).to_be_bytes());
            }
        };
    }
    macro_rules! finalize_response {
        () => {
            match proto {
                ModbusProto::Rtu => {
                    let crc = calc_rtu_crc(&response.as_slice(), response.len() as u8);
                    response.extend_from_slice(&crc.to_le_bytes());
                    Some(response)
                }
                ModbusProto::TcpUdp => Some(response),
            }
        };
    }
    if func >= 1 && func <= 4 {
        // funcs 1 - 4
        // read coils / registers
        if broadcast || !check_frame_crc!(6) {
            return None;
        }
        let count = u16::from_be_bytes([frame[start_frame + 4], frame[start_frame + 5]]);
        if ((func == 1 || func == 2) && count > 2000) || ((func == 3 || func == 4) && count > 125) {
            response_error!(0x03);
            return finalize_response!();
        }
        let reg = u16::from_be_bytes([frame[start_frame + 2], frame[start_frame + 3]]);
        let ctx = context::CONTEXT.lock().unwrap();
        let result = match func {
            1 => context::get_bools_as_u8(reg, count, &ctx.coils),
            2 => context::get_bools_as_u8(reg, count, &ctx.discretes),
            3 => context::get_regs_as_u8(reg, count, &ctx.holdings),
            4 => context::get_regs_as_u8(reg, count, &ctx.inputs),
            _ => panic!(), // never reaches
        };
        drop(ctx);
        match result {
            Ok(mut data) => {
                response_set_data_len!(data.len() + 3);
                // 2b unit and func
                response.extend_from_slice(&frame[start_frame..start_frame + 2]);
                response.push(data.len() as u8);
                response.append(&mut data);
                return finalize_response!();
            }
            Err(_) => {
                response_error!(0x02);
                return finalize_response!();
            }
        }
    } else if func == 5 {
        // func 5
        // write single coil
        if !check_frame_crc!(6) {
            return None;
        }
        let reg = u16::from_be_bytes([frame[start_frame + 2], frame[start_frame + 3]]);
        let val: bool;
        match u16::from_be_bytes([frame[start_frame + 4], frame[start_frame + 5]]) {
            0xff00 => val = true,
            0x0000 => val = false,
            _ => {
                if broadcast {
                    return None;
                } else {
                    response_error!(0x03);
                    return finalize_response!();
                }
            }
        };
        let result = context::set(reg, val, &mut context::CONTEXT.lock().unwrap().coils);
        if broadcast {
            return None;
        } else if result.is_err() {
            response_error!(0x02);
            return finalize_response!();
        } else {
            response_set_data_len!(6);
            // 6b unit, func, reg, val
            response.extend_from_slice(&frame[start_frame..start_frame + 6]);
            return finalize_response!();
        }
    } else if func == 6 {
        // func 6
        // write single register
        if !check_frame_crc!(6) {
            return None;
        }
        let reg = u16::from_be_bytes([frame[start_frame + 2], frame[start_frame + 3]]);
        let val = u16::from_be_bytes([frame[start_frame + 4], frame[start_frame + 5]]);
        let result = context::set(reg, val, &mut context::CONTEXT.lock().unwrap().holdings);
        if broadcast {
            return None;
        } else if result.is_err() {
            response_error!(0x02);
            return finalize_response!();
        } else {
            response_set_data_len!(6);
            // 6b unit, func, reg, val
            response.extend_from_slice(&frame[start_frame..start_frame + 6]);
            return finalize_response!();
        }
    } else if func == 0x0f || func == 0x10 {
        // funcs 15 & 16
        // write multiple coils / registers
        let bytes = frame[start_frame + 6];
        if !check_frame_crc!(7 + bytes) {
            return None;
        }
        if bytes > 242 {
            if broadcast {
                return None;
            } else {
                response_error!(0x03);
                return finalize_response!();
            }
        }
        let reg = u16::from_be_bytes([frame[start_frame + 2], frame[start_frame + 3]]);
        let count = u16::from_be_bytes([frame[start_frame + 4], frame[start_frame + 5]]);
        let mut data: Vec<u8> = Vec::new();
        data.extend_from_slice(&frame[start_frame + 7..start_frame + 7 + bytes as usize]);
        let result = match func {
            0x0f => context::set_bools_from_u8(
                reg,
                count,
                &data,
                &mut context::CONTEXT.lock().unwrap().coils,
            ),
            0x10 => context::set_regs_from_u8(
                reg,
                &data,
                &mut context::CONTEXT.lock().unwrap().holdings,
            ),
            _ => panic!(), // never reaches
        };
        if broadcast {
            return None;
        } else {
            match result {
                Ok(_) => {
                    response_set_data_len!(6);
                    // 6b unit, f, reg, cnt
                    response.extend_from_slice(&frame[start_frame..start_frame + 6]);
                    return finalize_response!();
                }
                Err(_) => {
                    response_error!(0x02);
                    return finalize_response!();
                }
            }
        }
    } else {
        response_error!(0x01);
        return finalize_response!();
    }
}
