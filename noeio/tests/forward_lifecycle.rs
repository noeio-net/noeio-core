//! AC-19: the forwarding exists exactly as long as the `noeio forward`
//! process does. A real `noeio forward` child is started (no daemon, no
//! root: the debug-only `NOEIO_FORWARD_HOST_ADDRS` hook supplies the address
//! snapshot), a connection is kept open through it, the process is ended in
//! every way the requirements list, and afterwards the client socket must be
//! closed and the port must be free again.
//!
//! The listener is bound to one of this machine's real non-loopback IPv4
//! addresses (validation refuses loopback on purpose). Without such an
//! address the tests are skipped.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const HOST_ADDRS_ENV: &str = "NOEIO_FORWARD_HOST_ADDRS";

/// The address the OS would use to reach the outside world. No packet is
/// sent: `connect` on a UDP socket only selects a route.
fn primary_lan_addr() -> Option<Ipv4Addr> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    sock.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_loopback() && !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

fn free_port(ip: Ipv4Addr) -> u16 {
    TcpListener::bind((ip, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A loopback echo server that lives for the whole test.
fn echo_server() -> SocketAddrV4 {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = match l.local_addr().unwrap() {
        SocketAddr::V4(a) => a,
        _ => unreachable!(),
    };
    std::thread::spawn(move || {
        for stream in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = stream;
                let mut w = r.try_clone().unwrap();
                let mut buf = [0u8; 4096];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 || w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

struct Forward {
    child: Child,
    bind: SocketAddrV4,
    /// Whatever the child prints after the banner (the exit summary).
    summary: std::sync::mpsc::Receiver<Vec<String>>,
}

impl Forward {
    fn take_summary(&self) -> Vec<String> {
        self.summary
            .recv_timeout(Duration::from_secs(3))
            .unwrap_or_default()
    }
}

/// A loopback UDP echo server that lives for the whole test.
fn udp_echo_server() -> SocketAddrV4 {
    let s = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = match s.local_addr().unwrap() {
        SocketAddr::V4(a) => a,
        _ => unreachable!(),
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = s.recv_from(&mut buf) {
            let _ = s.send_to(&buf[..n], from);
        }
    });
    addr
}

/// Start `noeio forward --listen <lan_ip>:<port> --target <echo>` and wait
/// for the banner's last line, i.e. for the listener to be up.
fn start_forward(lan: Ipv4Addr) -> Forward {
    start_forward_with(lan, "tcp", echo_server())
}

fn start_forward_with(lan: Ipv4Addr, proto: &str, target: SocketAddrV4) -> Forward {
    let bind = SocketAddrV4::new(lan, free_port(lan));
    let mut child = Command::new(env!("CARGO_BIN_EXE_noeio"))
        .args([
            "forward",
            "--listen",
            &bind.to_string(),
            "--target",
            &target.to_string(),
            "--proto",
            proto,
        ])
        // The overlay address must differ from `lan`, otherwise the bind is
        // classified as overlay-side; the value itself is never bound.
        .env(HOST_ADDRS_ENV, format!("overlay=110.20.0.1;lan={lan}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn noeio forward");

    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "banner did not appear");
        let line = lines.next().expect("stdout closed before banner").unwrap();
        if line == "press Ctrl+C to stop" {
            break;
        }
    }
    // Keep draining stdout so the child never blocks on a full pipe; the
    // summary is collected for the graceful cases.
    let (tx, summary) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rest: Vec<String> = lines.map_while(Result::ok).collect();
        let _ = tx.send(rest);
    });
    Forward {
        child,
        bind,
        summary,
    }
}

/// Open a connection through the forwarder and prove it is live.
fn live_connection(bind: SocketAddrV4) -> TcpStream {
    let mut c = TcpStream::connect_timeout(&SocketAddr::V4(bind), Duration::from_secs(2)).unwrap();
    c.write_all(b"hello").unwrap();
    let mut buf = [0u8; 5];
    c.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello");
    c
}

fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "process did not exit within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// After the process is gone the client must see EOF or a reset, and the
/// port must be bindable again within a second.
fn assert_released(mut client: TcpStream, bind: SocketAddrV4) {
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut buf = [0u8; 16];
    match client.read(&mut buf) {
        Ok(0) => {}
        Ok(n) => panic!("client still received {n} bytes"),
        Err(err) => assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ),
            "unexpected error {err:?}"
        ),
    }

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match TcpListener::bind(bind) {
            Ok(_) => return,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("port {bind} still taken 1s after exit: {err}"),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &Child, sig: &str) {
    let status = Command::new("kill")
        .args([format!("-{sig}"), child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{sig} failed");
}

/// SIGINT / SIGTERM / SIGHUP: graceful exit, code 0, summary printed.
#[cfg(unix)]
fn graceful(sig: &str) {
    let Some(lan) = primary_lan_addr() else {
        eprintln!("skipping: no non-loopback IPv4 address on this machine");
        return;
    };
    let mut fwd = start_forward(lan);
    let client = live_connection(fwd.bind);

    send_signal(&fwd.child, sig);
    let status = wait_exit(&mut fwd.child, Duration::from_secs(2));
    assert_eq!(status.code(), Some(0), "{sig}: exit code");

    let summary = fwd.take_summary();
    assert!(
        summary
            .iter()
            .any(|l| l.starts_with("shutting down: 1 listener closed, 1 connection aborted")),
        "{sig}: summary missing or wrong: {summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|l| l.starts_with("total: 1 connections")),
        "{sig}: counters: {summary:?}"
    );
    assert_released(client, fwd.bind);
}

#[cfg(unix)]
#[test]
fn sigint_ends_forwarding() {
    graceful("INT");
}

#[cfg(unix)]
#[test]
fn sigterm_ends_forwarding() {
    graceful("TERM");
}

#[cfg(unix)]
#[test]
fn sighup_ends_forwarding() {
    graceful("HUP");
}

/// SIGKILL (Unix) / TerminateProcess (Windows): no summary, but the kernel
/// releases the listener and the connection all the same (FR-3.4).
#[test]
fn hard_kill_leaves_nothing_behind() {
    let Some(lan) = primary_lan_addr() else {
        eprintln!("skipping: no non-loopback IPv4 address on this machine");
        return;
    };
    let mut fwd = start_forward(lan);
    let client = live_connection(fwd.bind);

    fwd.child.kill().unwrap();
    let status = wait_exit(&mut fwd.child, Duration::from_secs(2));
    assert!(!status.success());
    let summary = fwd.take_summary();
    assert!(
        summary.is_empty(),
        "a hard kill cannot print a summary: {summary:?}"
    );
    assert_released(client, fwd.bind);
}

/// FR-3.7 / AC-10: two forwarders are independent processes.
#[test]
fn processes_are_independent() {
    let Some(lan) = primary_lan_addr() else {
        eprintln!("skipping: no non-loopback IPv4 address on this machine");
        return;
    };
    let mut a = start_forward(lan);
    let mut b = start_forward(lan);

    let ca = live_connection(a.bind);
    a.child.kill().unwrap();
    wait_exit(&mut a.child, Duration::from_secs(2));
    assert_released(ca, a.bind);

    // b is untouched.
    let _cb = live_connection(b.bind);
    b.child.kill().unwrap();
    wait_exit(&mut b.child, Duration::from_secs(2));
}

/// AC-13 shape: no daemon → refuse to start, exit 1, the hint is printed.
/// In debug builds the env hook takes over, so only run this when it is
/// unset and the daemon is genuinely unreachable (no root here).
#[test]
fn without_daemon_the_command_refuses_to_start() {
    let out = Command::new(env!("CARGO_BIN_EXE_noeio"))
        .args([
            "forward",
            "--listen",
            "noeio:8080",
            "--target",
            "192.168.10.7:80",
        ])
        .env_remove(HOST_ADDRS_ENV)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Is `noeio boot` running?"),
        "stderr: {stderr}"
    );
}

/// FR-1.3 at the process level: every rule error is printed, exit 2.
#[test]
fn invalid_rule_exits_with_usage_code() {
    let out = Command::new(env!("CARGO_BIN_EXE_noeio"))
        .args([
            "forward",
            "--listen",
            "0.0.0.0:8080",
            "--target",
            "nas.local:80",
            "--allow-from",
            "10.0.0.0/8,junk",
        ])
        .env(HOST_ADDRS_ENV, "overlay=110.20.0.1;lan=192.168.10.3")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("0.0.0.0"), "{stderr}");
    assert!(stderr.contains("hostnames are not accepted"), "{stderr}");
    assert!(
        stderr.contains("'junk' is not a valid IPv4 CIDR"),
        "{stderr}"
    );
}

/// FR-4 at the process level: a UDP rule relays datagrams, and ending the
/// process frees the port. On Unix the exit is graceful (SIGTERM) and the
/// summary reports the session it dropped; elsewhere the child is killed
/// and only the release is checked.
#[test]
fn udp_forward_relays_and_exits() {
    let Some(lan) = primary_lan_addr() else {
        eprintln!("skipping: no non-loopback IPv4 address on this machine");
        return;
    };
    let mut fwd = start_forward_with(lan, "udp", udp_echo_server());

    let client = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    for i in 0..3u8 {
        let msg = [i; 32];
        client.send_to(&msg, fwd.bind).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], &msg, "udp echo through the forwarder");
    }

    #[cfg(unix)]
    {
        send_signal(&fwd.child, "TERM");
        let status = wait_exit(&mut fwd.child, Duration::from_secs(2));
        assert_eq!(status.code(), Some(0));
        let summary = fwd.take_summary();
        assert!(
            summary
                .iter()
                .any(|l| l == "shutting down: 1 listener closed, 1 session dropped"),
            "{summary:?}"
        );
        assert!(
            summary.iter().any(|l| l.starts_with("total: 1 sessions")),
            "{summary:?}"
        );
    }
    #[cfg(not(unix))]
    {
        fwd.child.kill().unwrap();
        let status = wait_exit(&mut fwd.child, Duration::from_secs(2));
        assert!(!status.success());
        assert!(fwd.take_summary().is_empty());
    }
    // The port is free again.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match UdpSocket::bind(fwd.bind) {
            Ok(_) => break,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("udp port {} still taken: {err}", fwd.bind),
        }
    }
}
