use super::{Fallible, Id, check_profile_file, save};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufReader, Read},
    net::SocketAddrV4,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const ROUTE_FILE_LIMIT: usize = 8192;
const WRITE_RETRY_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct PeerRoutes {
    path: PathBuf,
    capacity: usize,
    addresses: BTreeMap<Id, SocketAddrV4>,
    can_save: bool,
    retry_at: Option<Instant>,
    pub(crate) warning: Option<String>,
}

impl PeerRoutes {
    pub(crate) fn open(path: PathBuf, capacity: usize) -> Self {
        let (addresses, warning) = match read_routes(&path, capacity) {
            Ok(addresses) => (addresses, None),
            Err(error) => (
                BTreeMap::new(),
                Some(format!(
                    "Не удалось прочитать сохранённые маршруты. {error}"
                )),
            ),
        };
        Self {
            path,
            capacity,
            addresses,
            can_save: warning.is_none(),
            retry_at: None,
            warning,
        }
    }

    pub(crate) fn addresses(&self) -> impl Iterator<Item = SocketAddrV4> + '_ {
        self.addresses.values().copied()
    }

    pub(crate) fn remember(&mut self, device: Id, address: SocketAddrV4) {
        let changed = match self.addresses.get(&device) {
            Some(current) => *current != address,
            None => self.addresses.len() < self.capacity,
        };
        if changed {
            self.addresses.insert(device, address);
        }
        let now = Instant::now();
        if !self.can_save || !self.retry_at.map_or(changed, |retry| retry <= now) {
            return;
        }
        match save(&self.path, &self.addresses, ROUTE_FILE_LIMIT) {
            Ok(_) => {
                self.warning = None;
                self.retry_at = None;
            }
            Err(error) => {
                self.warning = Some(format!(
                    "Не удалось сохранить адрес для следующего запуска. {error}"
                ));
                self.retry_at = Some(now + WRITE_RETRY_INTERVAL);
            }
        }
    }
}

fn read_routes(path: &Path, capacity: usize) -> Fallible<BTreeMap<Id, SocketAddrV4>> {
    check_profile_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    if file.metadata()?.len() > ROUTE_FILE_LIMIT as u64 {
        return Err("Файл маршрутов превышает допустимый размер.".into());
    }
    let mut limited = file.take(ROUTE_FILE_LIMIT as u64 + 1);
    let routes: BTreeMap<_, _> = serde_json::from_reader(BufReader::new(&mut limited))?;
    if limited.limit() == 0 || routes.len() > capacity {
        return Err("Файл маршрутов превышает допустимый размер.".into());
    }
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, net::Ipv4Addr};

    #[test]
    fn known_device_replaces_its_route_at_capacity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routes.json");
        let address = |port| SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        let mut routes = PeerRoutes::open(path.clone(), 2);
        routes.remember(Id::from_u128(1), address(1000));
        routes.remember(Id::from_u128(2), address(1001));
        routes.remember(Id::from_u128(3), address(1002));
        routes.remember(Id::from_u128(1), address(1003));
        let reopened = PeerRoutes::open(path, 2);
        assert!(reopened.warning.is_none());
        assert_eq!(
            reopened.addresses().collect::<Vec<_>>(),
            [address(1003), address(1001)]
        );
    }

    #[test]
    fn unreadable_route_file_does_not_prevent_learning_in_memory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routes.json");
        fs::write(&path, b"{ damaged").unwrap();
        let mut routes = PeerRoutes::open(path.clone(), 2);
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1000);
        routes.remember(Id::from_u128(1), address);
        assert_eq!(routes.addresses().collect::<Vec<_>>(), [address]);
        assert!(routes.warning.is_some());
        assert_eq!(fs::read(path).unwrap(), b"{ damaged");
    }

    #[test]
    fn same_address_is_saved_after_a_temporary_write_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routes.json");
        let mut routes = PeerRoutes::open(path.clone(), 2);
        let device = Id::from_u128(1);
        let address = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1000);
        fs::create_dir(&path).unwrap();
        routes.remember(device, address);
        assert!(routes.warning.is_some());
        assert_eq!(routes.addresses().collect::<Vec<_>>(), [address]);
        fs::remove_dir(&path).unwrap();

        routes.retry_at = Some(Instant::now());
        routes.remember(device, address);
        assert!(routes.warning.is_none());
        assert_eq!(
            PeerRoutes::open(path, 2).addresses().collect::<Vec<_>>(),
            [address]
        );
    }
}
