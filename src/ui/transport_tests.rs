use super::*;
use godovoy_plan::storage::Store;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use tempfile::tempdir;

#[test]
fn empty_profile_waits_to_listen_and_existing_calendar_starts_normally() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("profile");
    let probe = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, probe.local_addr().unwrap().port());
    let config = Config {
        listen: addr,
        discovery_port: None,
    };
    let mut app = App::open(Store::open(&path).unwrap(), config, false);
    assert!(matches!(app.network, DesktopTransport::Waiting(_)));
    drop(probe);
    assert!(TcpStream::connect(addr).is_err());
    app.company = "Отдел".into();
    app.employee = "Анна".into();
    app.create();
    assert!(TcpStream::connect(addr).is_err());
    drop(app);
    let mut app = App::open(Store::open(&path).unwrap(), config, false);
    assert!(TcpStream::connect(addr).is_err());
    app.network.enable(&app.store).unwrap();
    assert!(TcpStream::connect(addr).is_ok());
    drop(app);
    let _existing = App::open(Store::open(&path).unwrap(), config, false);
    assert!(TcpStream::connect(addr).is_ok());
}
