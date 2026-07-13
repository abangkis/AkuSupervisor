#![allow(unsafe_code)]

use std::fmt;
use std::io;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::slice;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID,
    MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

use crate::application::{NetworkFamily, PortDiagnostic, PortInspector, PortOccupant};

const QUERY_ATTEMPTS: usize = 4;
const TABLE_HEADER_BYTES: u32 = 4;

/// Stateless Windows implementation of read-only TCP port diagnostics.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsPortInspector;

impl PortInspector for WindowsPortInspector {
    type Error = PortObserverError;

    fn inspect_tcp_port(&self, port: u16) -> Result<PortDiagnostic, Self::Error> {
        inspect_tcp_port(port)
    }
}

/// Queries the Windows TCP tables for processes currently using `port`.
///
/// The function is observational: it opens no process handle and exposes no
/// mutation or termination operation.
///
/// # Errors
///
/// Returns an operating-system query error if either TCP table cannot be read,
/// or a malformed-table error if Windows returns inconsistent buffer metadata.
pub fn inspect_tcp_port(port: u16) -> Result<PortDiagnostic, PortObserverError> {
    let mut occupants = Vec::new();

    inspect_ipv4(port, &mut occupants)?;
    inspect_ipv6(port, &mut occupants)?;

    occupants.sort_unstable();
    occupants.dedup();

    Ok(PortDiagnostic::new(port, occupants))
}

fn inspect_ipv4(port: u16, occupants: &mut Vec<PortOccupant>) -> Result<(), PortObserverError> {
    let buffer = query_tcp_table(u32::from(AF_INET))?;
    let table = buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>();
    let count = unsafe { (*table).dwNumEntries as usize };
    let rows_offset = offset_of!(MIB_TCPTABLE_OWNER_PID, table);
    validate_rows::<MIB_TCPROW_OWNER_PID>(buffer.byte_len, rows_offset, count)?;

    let rows_pointer = unsafe { ptr::addr_of!((*table).table).cast::<MIB_TCPROW_OWNER_PID>() };
    let rows = unsafe { slice::from_raw_parts(rows_pointer, count) };
    occupants.extend(
        rows.iter()
            .filter(|row| decode_port(row.dwLocalPort) == port)
            .map(|row| PortOccupant::new(row.dwOwningPid, NetworkFamily::V4)),
    );

    Ok(())
}

fn inspect_ipv6(port: u16, occupants: &mut Vec<PortOccupant>) -> Result<(), PortObserverError> {
    let buffer = query_tcp_table(u32::from(AF_INET6))?;
    let table = buffer.as_ptr().cast::<MIB_TCP6TABLE_OWNER_PID>();
    let count = unsafe { (*table).dwNumEntries as usize };
    let rows_offset = offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
    validate_rows::<MIB_TCP6ROW_OWNER_PID>(buffer.byte_len, rows_offset, count)?;

    let rows_pointer = unsafe { ptr::addr_of!((*table).table).cast::<MIB_TCP6ROW_OWNER_PID>() };
    let rows = unsafe { slice::from_raw_parts(rows_pointer, count) };
    occupants.extend(
        rows.iter()
            .filter(|row| decode_port(row.dwLocalPort) == port)
            .map(|row| PortOccupant::new(row.dwOwningPid, NetworkFamily::V6)),
    );

    Ok(())
}

fn query_tcp_table(family: u32) -> Result<TableBuffer, PortObserverError> {
    let mut byte_len = 0_u32;
    let first_status = unsafe {
        GetExtendedTcpTable(
            ptr::null_mut(),
            &raw mut byte_len,
            0,
            family,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };

    if first_status != ERROR_INSUFFICIENT_BUFFER && first_status != NO_ERROR {
        return Err(PortObserverError::Query(io::Error::from_raw_os_error(
            first_status.cast_signed(),
        )));
    }

    byte_len = byte_len.max(TABLE_HEADER_BYTES);
    for _ in 0..QUERY_ATTEMPTS {
        let word_count = (byte_len as usize).div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; word_count];
        let mut returned_len = byte_len;
        let status = unsafe {
            GetExtendedTcpTable(
                storage.as_mut_ptr().cast(),
                &raw mut returned_len,
                0,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };

        if status == NO_ERROR {
            if returned_len < TABLE_HEADER_BYTES
                || returned_len as usize > storage.len() * size_of::<usize>()
            {
                return Err(PortObserverError::MalformedTable);
            }
            return Ok(TableBuffer {
                storage,
                byte_len: returned_len as usize,
            });
        }
        if status != ERROR_INSUFFICIENT_BUFFER {
            return Err(PortObserverError::Query(io::Error::from_raw_os_error(
                status.cast_signed(),
            )));
        }
        byte_len = returned_len.max(byte_len.saturating_mul(2));
    }

    Err(PortObserverError::ChangingTable)
}

fn validate_rows<Row>(
    byte_len: usize,
    rows_offset: usize,
    count: usize,
) -> Result<(), PortObserverError> {
    let rows_len = count
        .checked_mul(size_of::<Row>())
        .and_then(|length| rows_offset.checked_add(length))
        .ok_or(PortObserverError::MalformedTable)?;
    if rows_len > byte_len {
        return Err(PortObserverError::MalformedTable);
    }
    Ok(())
}

const fn decode_port(raw_port: u32) -> u16 {
    let [first, second, _, _] = raw_port.to_le_bytes();
    u16::from_be_bytes([first, second])
}

#[derive(Debug)]
struct TableBuffer {
    storage: Vec<usize>,
    byte_len: usize,
}

impl TableBuffer {
    fn as_ptr(&self) -> *const usize {
        self.storage.as_ptr()
    }
}

/// Failure while collecting read-only TCP port diagnostics.
#[derive(Debug)]
pub enum PortObserverError {
    Query(io::Error),
    ChangingTable,
    MalformedTable,
}

impl fmt::Display for PortObserverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(error) => write!(formatter, "failed to query Windows TCP table: {error}"),
            Self::ChangingTable => {
                formatter.write_str("Windows TCP table changed during every query")
            }
            Self::MalformedTable => {
                formatter.write_str("Windows returned malformed TCP table metadata")
            }
        }
    }
}

impl std::error::Error for PortObserverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Query(error) => Some(error),
            Self::ChangingTable | Self::MalformedTable => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::process;
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::application::PortInspector;

    use super::{WindowsPortInspector, decode_port};

    const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn decodes_windows_network_byte_order_port() {
        assert_eq!(decode_port(0x0000_901f), 8_080);
    }

    #[test]
    fn occupied_port_reports_owner_without_disrupting_listener() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind diagnostic listener");
        let port = listener.local_addr().expect("listener address").port();
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;

        let diagnostic = loop {
            let diagnostic = WindowsPortInspector
                .inspect_tcp_port(port)
                .expect("inspect occupied port");
            if diagnostic
                .occupants()
                .iter()
                .any(|occupant| occupant.pid() == process::id())
            {
                break diagnostic;
            }
            assert!(Instant::now() < deadline, "listener owner was not observed");
            thread::sleep(Duration::from_millis(20));
        };

        assert_eq!(diagnostic.port(), port);
        assert!(!diagnostic.is_available());

        let client = TcpStream::connect(("127.0.0.1", port)).expect("listener remains reachable");
        let (server, _) = listener.accept().expect("listener remains able to accept");
        drop((client, server));
    }
}
