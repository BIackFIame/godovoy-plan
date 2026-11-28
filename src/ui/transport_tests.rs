use super::*;
use godovoy_plan::storage::Store;
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use tempfile::tempdir;

#[test]
fn empty_profile_waits_to_listen_and_existing_calendar_starts_normally() {
    let tmp = tempdir().unwrap();
    let path = tmp.path().join("profile");
    let config = Config {
        listen: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        discovery_port: None,
    };
    let mut app = App::open(Store::open(&path).unwrap(), config, false);
    assert!(matches!(app.network, DesktopTransport::Waiting(_)));
    app.company = "Отдел".into();
    app.employee = "Анна".into();
    app.create();
    assert!(matches!(app.network, DesktopTransport::Waiting(_)));
    drop(app);
    let mut app = App::open(Store::open(&path).unwrap(), config, false);
    assert!(matches!(app.network, DesktopTransport::Waiting(_)));
    let address = app.network.enable(&app.store).unwrap().address().unwrap();
    TcpStream::connect(address).unwrap();
    drop(app);
    let existing = App::open(Store::open(&path).unwrap(), config, false);
    let address = existing.network.as_ref().unwrap().address().unwrap();
    TcpStream::connect(address).unwrap();
}
