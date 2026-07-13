#![cfg(windows)]

use std::net::{TcpListener, TcpStream};
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use aku_supervisor::platform::windows::inspect_tcp_port;

const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn occupied_port_reports_owner_without_disrupting_listener() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind diagnostic listener");
    let port = listener.local_addr().expect("listener address").port();
    let deadline = Instant::now() + OBSERVATION_TIMEOUT;

    let diagnostic = loop {
        let diagnostic = inspect_tcp_port(port).expect("inspect occupied port");
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
