//! Выбор владельца на настоящих `flock` и сокетах во временном профиле.
//!
//! Каталоги профилей лежат в `/tmp`: путь временного каталога macOS по умолчанию
//! (`/var/folders/…`) вместе с именем сокета длиннее 103 байт.

use std::error::Error;
use std::ffi::OsString;
use std::fs::{self, File, Permissions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::{UnixListener as StdListener, UnixStream as StdStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Duration;

use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
use shellio_control::test_support::ManualClock;
use shellio_control::{
    Clock, Expired, InstanceError, LOCK_FILE, MAX_SOCKET_PATH, NodeKind, Ownership, Probe,
    SOCKET_FILE, SocketPaths, admit_peer, check_socket_path, is_own_uid, lock_path, probe_owner,
    socket_path,
};
use shellio_store::Profile;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

/// Временный каталог с коротким путём в `/tmp`.
fn short_tempdir() -> TestResult<TempDir> {
    Ok(tempfile::Builder::new()
        .prefix("shellio-")
        .tempdir_in("/tmp")?)
}

/// Профиль с созданным каталогом состояния `<tmp>/state`.
fn profile_in(dir: &TempDir) -> TestResult<Profile> {
    let profile = Profile::isolated(dir.path().join("state"));
    profile.ensure_state_dir()?;
    Ok(profile)
}

/// Профиль, путь каталога состояния которого занимает ровно `len` байт.
fn profile_with_len(dir: &TempDir, len: usize) -> Profile {
    let base = dir.path().as_os_str().len();
    assert!(
        len > base + 1,
        "каталог {} слишком длинный",
        dir.path().display()
    );
    let state_dir = dir.path().join("s".repeat(len - base - 1));
    assert_eq!(state_dir.as_os_str().len(), len);
    Profile::isolated(state_dir)
}

fn acquire(profile: &Profile) -> TestResult<Ownership> {
    Ok(Ownership::try_acquire(profile)?.ok_or("блокировка занята")?)
}

fn temporary_path(profile: &Profile) -> PathBuf {
    SocketPaths::new(profile, std::process::id())
        .temporary()
        .to_path_buf()
}

/// Сокет, оставшийся после прежнего владельца: `drop` слушателя путь не удаляет.
fn leave_socket(path: &Path) -> TestResult<u64> {
    drop(StdListener::bind(path)?);
    let metadata = fs::symlink_metadata(path)?;
    assert!(metadata.file_type().is_socket());
    Ok(metadata.ino())
}

fn mode(path: &Path) -> TestResult<u32> {
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o777)
}

fn digits(value: u32) -> usize {
    value.to_string().len()
}

#[test]
fn socket_path_length_boundary() {
    let fits = PathBuf::from(format!("/tmp/{}", "a".repeat(MAX_SOCKET_PATH - 5)));
    assert_eq!(fits.as_os_str().len(), 103);
    check_socket_path(&fits).unwrap();

    let long = PathBuf::from(format!("/tmp/{}", "a".repeat(MAX_SOCKET_PATH - 4)));
    assert_eq!(long.as_os_str().len(), 104);
    match check_socket_path(&long) {
        Err(InstanceError::SocketPathTooLong { path, len }) => {
            assert_eq!(path, long);
            assert_eq!(len, 104);
        }
        other => panic!("ожидался отказ длины, получено {other:?}"),
    }
}

#[test]
fn published_and_temporary_lengths_are_checked_separately() {
    let pid = 4242;
    let dir = short_tempdir().unwrap();
    // Конечное имя `<dir>/shellio.sock` длиннее каталога на 13 байт.
    let published_at = |len: usize| {
        let profile = profile_with_len(&dir, len - 1 - SOCKET_FILE.len());
        SocketPaths::new(&profile, pid)
    };

    let published_fits = published_at(103);
    assert_eq!(published_fits.published().as_os_str().len(), 103);
    match published_fits.check() {
        Err(InstanceError::SocketPathTooLong { path, .. }) => {
            assert_eq!(
                path,
                published_fits.temporary(),
                "конечное имя в 103 байта проходит"
            );
        }
        other => panic!("ожидался отказ временного имени, получено {other:?}"),
    }

    let published_long = published_at(104);
    match published_long.check() {
        Err(InstanceError::SocketPathTooLong { path, len }) => {
            assert_eq!(path, published_long.published());
            assert_eq!(len, 104);
        }
        other => panic!("ожидался отказ конечного имени, получено {other:?}"),
    }

    // Временное имя `<dir>/.shellio.sock.<pid>` длиннее каталога на 15 байт и цифры PID.
    let temporary_at = |len: usize| {
        let profile = profile_with_len(&dir, len - 15 - digits(pid));
        SocketPaths::new(&profile, pid)
    };
    let temporary_fits = temporary_at(103);
    assert_eq!(temporary_fits.temporary().as_os_str().len(), 103);
    temporary_fits.check().unwrap();

    let temporary_long = temporary_at(104);
    match temporary_long.check() {
        Err(InstanceError::SocketPathTooLong { path, len }) => {
            assert_eq!(path, temporary_long.temporary());
            assert_eq!(len, 104);
        }
        other => panic!("ожидался отказ временного имени, получено {other:?}"),
    }
}

#[test]
fn publish_respects_temporary_length_boundary() {
    let pid = std::process::id();
    let dir = short_tempdir().unwrap();
    let dir_len_for = |temporary_len: usize| temporary_len - 15 - digits(pid);

    let fits = profile_with_len(&dir, dir_len_for(103));
    fits.ensure_state_dir().unwrap();
    assert_eq!(temporary_path(&fits).as_os_str().len(), 103);
    let mut owner = acquire(&fits).unwrap();
    let listener = owner.publish().unwrap();
    assert!(
        fs::symlink_metadata(socket_path(&fits))
            .unwrap()
            .file_type()
            .is_socket()
    );
    StdStream::connect(socket_path(&fits)).unwrap();
    drop(listener);
    owner.release().unwrap();

    let long_dir = short_tempdir().unwrap();
    let long = profile_with_len(&long_dir, dir_len_for(104));
    long.ensure_state_dir().unwrap();
    let stale = leave_socket(&socket_path(&long)).unwrap();
    let mut owner = acquire(&long).unwrap();
    match owner.publish() {
        Err(InstanceError::SocketPathTooLong { path, len }) => {
            assert_eq!(path, temporary_path(&long));
            assert_eq!(len, 104);
        }
        other => panic!("ожидался отказ длины, получено {other:?}"),
    }
    assert_eq!(
        fs::symlink_metadata(socket_path(&long)).unwrap().ino(),
        stale,
        "отказ длины идёт до удаления оставшихся сокетов"
    );
    assert!(!temporary_path(&long).exists());
}

#[test]
fn default_profile_socket_paths_fit() {
    let home = OsString::from(format!("/Users/{}", "u".repeat(32)));
    let profile = Profile::resolve(|name| (name == "HOME").then(|| home.clone())).unwrap();
    // Наибольший PID macOS — 99 999.
    SocketPaths::new(&profile, 99_999).check().unwrap();
}

#[test]
fn two_claimants_yield_one_owner() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let claimants: Vec<_> = (0..2)
        .map(|_| {
            let profile = profile.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                Ownership::try_acquire(&profile).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<Option<Ownership>> = claimants
        .into_iter()
        .map(|claimant| claimant.join().unwrap())
        .collect();
    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_some()).count(),
        1
    );
    assert!(Ownership::try_acquire(&profile).unwrap().is_none());
}

#[test]
fn lock_inode_survives_release_and_reacquire() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let lock = lock_path(&profile);

    let owner = acquire(&profile).unwrap();
    let inode = fs::metadata(&lock).unwrap().ino();
    owner.release().unwrap();
    assert_eq!(
        fs::metadata(&lock).unwrap().ino(),
        inode,
        "файл блокировки на месте"
    );
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);

    let owner = acquire(&profile).unwrap();
    assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
    drop(owner);
    assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
    let owner = acquire(&profile).unwrap();
    assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
    drop(owner);
}

#[test]
fn probe_without_lock_file_creates_nothing() {
    let dir = short_tempdir().unwrap();
    let missing = Profile::isolated(dir.path().join("state"));
    assert_eq!(probe_owner(&missing).unwrap(), Probe::NotRunning);
    assert!(
        !missing.state_dir().exists(),
        "проба не создаёт каталог состояния"
    );

    let profile = profile_in(&dir).unwrap();
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
    assert!(
        !lock_path(&profile).exists(),
        "проба не создаёт файл блокировки"
    );
    assert_eq!(fs::read_dir(profile.state_dir()).unwrap().count(), 0);
}

#[test]
fn probe_reports_free_and_held_lock() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();

    let mut owner = acquire(&profile).unwrap();
    let _listener = owner.publish().unwrap();
    assert_eq!(probe_owner(&profile).unwrap(), Probe::Held);
    assert!(
        Ownership::try_acquire(&profile).unwrap().is_none(),
        "проба не отпускает чужую блокировку"
    );
    assert!(socket_path(&profile).exists(), "проба не трогает сокет");

    owner.release().unwrap();
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
    acquire(&profile).unwrap().release().unwrap();
}

#[test]
fn lock_and_socket_are_private() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let mut owner = acquire(&profile).unwrap();
    let _listener = owner.publish().unwrap();
    assert_eq!(mode(&lock_path(&profile)).unwrap(), 0o600);
    let socket = socket_path(&profile);
    assert!(
        fs::symlink_metadata(&socket)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(mode(&socket).unwrap(), 0o600);
    owner.release().unwrap();

    let other = short_tempdir().unwrap();
    let profile = profile_in(&other).unwrap();
    let lock = lock_path(&profile);
    fs::write(&lock, b"").unwrap();
    fs::set_permissions(&lock, Permissions::from_mode(0o644)).unwrap();
    let owner = acquire(&profile).unwrap();
    assert_eq!(
        mode(&lock).unwrap(),
        0o600,
        "владелец сужает права оставшегося файла"
    );
    drop(owner);
}

#[derive(Clone, Copy, Debug)]
enum Squatter {
    File,
    Symlink,
}

impl Squatter {
    fn place(self, path: &Path, target: &Path) -> io::Result<()> {
        match self {
            Self::File => fs::write(path, b"not a socket"),
            Self::Symlink => symlink(target, path),
        }
    }

    fn kind(self) -> NodeKind {
        match self {
            Self::File => NodeKind::File,
            Self::Symlink => NodeKind::Symlink,
        }
    }
}

#[test]
fn non_socket_at_socket_names_refuses_without_removal() {
    for squatter in [Squatter::File, Squatter::Symlink] {
        for temporary in [false, true] {
            let dir = short_tempdir().unwrap();
            let profile = profile_in(&dir).unwrap();
            let occupied = if temporary {
                temporary_path(&profile)
            } else {
                socket_path(&profile)
            };
            // Ссылка указывает на живой сокет: удаляется только сам сокет, не ссылка на него.
            let target = profile.state_dir().join("target.sock");
            let target_listener = StdListener::bind(&target).unwrap();
            squatter.place(&occupied, &target).unwrap();
            let stale = profile.state_dir().join(".shellio.sock.1");
            let stale_inode = leave_socket(&stale).unwrap();

            let mut owner = acquire(&profile).unwrap();
            match owner.publish() {
                Err(InstanceError::Occupied { path, kind }) => {
                    assert_eq!(path, occupied, "{squatter:?}, temporary = {temporary}");
                    assert_eq!(kind, squatter.kind());
                }
                other => panic!("{squatter:?}, temporary = {temporary}: получено {other:?}"),
            }
            let metadata = fs::symlink_metadata(&occupied).unwrap();
            match squatter {
                Squatter::File => {
                    assert!(metadata.file_type().is_file());
                    assert_eq!(fs::read(&occupied).unwrap(), b"not a socket");
                }
                Squatter::Symlink => {
                    assert!(metadata.file_type().is_symlink());
                    assert_eq!(fs::read_link(&occupied).unwrap(), target);
                }
            }
            assert_eq!(
                fs::symlink_metadata(&stale).unwrap().ino(),
                stale_inode,
                "отказ идёт до удаления оставшихся сокетов"
            );
            assert!(target.exists());
            if temporary {
                assert!(!socket_path(&profile).exists());
            }
            drop(target_listener);
            owner.release().unwrap();
        }
    }
}

#[test]
fn non_socket_under_foreign_temporary_name_refuses() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let foreign = profile.state_dir().join(".shellio.sock.1");
    fs::write(&foreign, b"").unwrap();
    let mut owner = acquire(&profile).unwrap();
    match owner.publish() {
        Err(InstanceError::Occupied { path, kind }) => {
            assert_eq!(path, foreign);
            assert_eq!(kind, NodeKind::File);
        }
        other => panic!("получено {other:?}"),
    }
    assert!(foreign.exists());
}

#[test]
fn leftover_sockets_are_replaced_by_new_owner_only() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let published = socket_path(&profile);
    let temporary = temporary_path(&profile);
    let foreign = profile.state_dir().join(".shellio.sock.99999");
    // Прежний владелец взял блокировку и был убит: файл блокировки и сокеты остались.
    acquire(&profile).unwrap().release().unwrap();
    let stale_published = leave_socket(&published).unwrap();
    let stale_temporary = leave_socket(&temporary).unwrap();
    let stale_foreign = leave_socket(&foreign).unwrap();
    let refused = StdStream::connect(&published).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::ConnectionRefused);

    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
    assert_eq!(
        fs::symlink_metadata(&published).unwrap().ino(),
        stale_published
    );
    assert_eq!(
        fs::symlink_metadata(&temporary).unwrap().ino(),
        stale_temporary
    );
    assert_eq!(fs::symlink_metadata(&foreign).unwrap().ino(), stale_foreign);

    let mut owner = acquire(&profile).unwrap();
    let _listener = owner.publish().unwrap();
    let metadata = fs::symlink_metadata(&published).unwrap();
    assert!(metadata.file_type().is_socket());
    assert_ne!(metadata.ino(), stale_published);
    assert!(!temporary.exists(), "временный сокет того же PID удалён");
    assert!(!foreign.exists());
    StdStream::connect(&published).unwrap();
    owner.release().unwrap();
}

#[test]
fn release_and_drop_remove_own_socket_only() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let socket = socket_path(&profile);
    let lock = lock_path(&profile);

    let mut owner = acquire(&profile).unwrap();
    let listener = owner.publish().unwrap();
    owner.release().unwrap();
    assert!(!socket.exists());
    assert!(lock.exists());
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);
    drop(listener);

    let mut owner = acquire(&profile).unwrap();
    let listener = owner.publish().unwrap();
    drop(owner);
    assert!(!socket.exists());
    assert!(lock.exists());
    drop(listener);

    // Чужой узел на месте своего сокета не удаляется.
    let mut owner = acquire(&profile).unwrap();
    let _listener = owner.publish().unwrap();
    fs::remove_file(&socket).unwrap();
    let foreign = leave_socket(&socket).unwrap();
    owner.release().unwrap();
    assert_eq!(fs::symlink_metadata(&socket).unwrap().ino(), foreign);
}

/// Код `/bin/test -e /dev/fd/<fd>` в потомке, порождённом без оболочки.
fn child_sees(fd: RawFd) -> TestResult<i32> {
    let status = Command::new("/bin/test")
        .arg("-e")
        .arg(format!("/dev/fd/{fd}"))
        .status()?;
    Ok(status.code().ok_or("потомок завершён сигналом")?)
}

#[tokio::test]
async fn descriptors_are_not_inherited() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let mut owner = acquire(&profile).unwrap();
    let listener = UnixListener::from_std(owner.publish().unwrap()).unwrap();
    let client = UnixStream::connect(socket_path(&profile)).await.unwrap();
    let (accepted, _) = listener.accept().await.unwrap();

    let descriptors = [
        ("блокировка", owner.as_fd()),
        ("слушатель", listener.as_fd()),
        ("принятый сокет", accepted.as_fd()),
        ("клиентский сокет", client.as_fd()),
    ];
    for (name, borrowed) in descriptors {
        let fd = borrowed.as_raw_fd();
        let flags = fcntl_getfd(borrowed).unwrap();
        assert!(
            flags.contains(FdFlags::CLOEXEC),
            "{name} ({fd}) без CLOEXEC"
        );
        assert_eq!(child_sees(fd).unwrap(), 1, "{name} ({fd}) открыт в потомке");
    }

    // Положительный контроль из плана: дескриптор, переданный на номер 0, виден потомку.
    let control = File::open(lock_path(&profile)).unwrap();
    let status = Command::new("/bin/test")
        .args(["-e", "/dev/fd/0"])
        .stdin(Stdio::from(control))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(0));

    // Контроль оракула на большом номере: копия без CLOEXEC видна потомку под своим номером,
    // поэтому код 1 выше значит закрытый в потомке номер, а не слепоту оракула.
    let inherited = File::open(lock_path(&profile))
        .unwrap()
        .try_clone()
        .unwrap();
    fcntl_setfd(&inherited, FdFlags::empty()).unwrap();
    assert_eq!(child_sees(inherited.as_raw_fd()).unwrap(), 0);
    drop(inherited);

    drop((client, accepted, listener));
    owner.release().unwrap();
}

#[tokio::test]
async fn same_user_peer_is_admitted() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let mut owner = acquire(&profile).unwrap();
    let listener = UnixListener::from_std(owner.publish().unwrap()).unwrap();
    let mut client = UnixStream::connect(socket_path(&profile)).await.unwrap();
    let (accepted, _) = listener.accept().await.unwrap();

    let own = accepted.peer_cred().unwrap().uid();
    assert!(is_own_uid(own));
    assert!(!is_own_uid(own.wrapping_add(1)));
    let mut admitted = admit_peer(accepted).unwrap();

    client.write_all(b"x").await.unwrap();
    let mut byte = [0u8; 1];
    admitted.read_exact(&mut byte).await.unwrap();
    assert_eq!(&byte, b"x");
    drop((client, admitted, listener));
    owner.release().unwrap();
}

/// Претендент, как второй запуск: проигрыш блокировки — ждать по часам и повторить.
async fn claim(profile: Profile, clock: ManualClock) -> TestResult<(Ownership, u32)> {
    let deadline = clock.deadline(Duration::from_secs(10));
    let mut attempts = 0;
    loop {
        attempts += 1;
        if let Some(owner) = Ownership::try_acquire(&profile)? {
            return Ok((owner, attempts));
        }
        clock.wait_retry(deadline).await?;
    }
}

#[tokio::test]
async fn claimant_retries_past_client_probe() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    acquire(&profile).unwrap().release().unwrap();

    // Тест держит разделяемую блокировку, как клиент в середине пробы.
    let probe = File::open(lock_path(&profile)).unwrap();
    probe.try_lock_shared().unwrap();

    let clock = ManualClock::new();
    let claimant = tokio::spawn(claim(profile.clone(), clock.clone()));
    clock.until_waiting(1).await;
    assert_eq!(probe_owner(&profile).unwrap(), Probe::NotRunning);

    probe.unlock().unwrap();
    drop(probe);
    clock.tick();
    let (owner, attempts) = claimant.await.unwrap().unwrap();
    assert_eq!(attempts, 2, "первая попытка проиграла пробе");
    assert_eq!(clock.waits(), 1);
    assert_eq!(probe_owner(&profile).unwrap(), Probe::Held);
    owner.release().unwrap();
}

#[tokio::test]
async fn claimant_gives_up_at_deadline_while_owner_lives() {
    let dir = short_tempdir().unwrap();
    let profile = profile_in(&dir).unwrap();
    let owner = acquire(&profile).unwrap();

    let clock = ManualClock::new();
    let claimant = tokio::spawn(claim(profile.clone(), clock.clone()));
    clock.until_waiting(1).await;
    clock.tick();
    clock.until_waiting(2).await;
    clock.expire();
    let refused = claimant.await.unwrap().err().unwrap();
    assert_eq!(refused.downcast_ref::<Expired>(), Some(&Expired));
    assert_eq!(clock.waits(), 2);
    owner.release().unwrap();
}

#[test]
fn lock_file_name_is_stable() {
    let profile = Profile::isolated("/state");
    assert_eq!(lock_path(&profile), Path::new("/state").join(LOCK_FILE));
    assert_eq!(socket_path(&profile), Path::new("/state/shellio.sock"));
    assert_eq!(
        SocketPaths::new(&profile, 77).temporary(),
        Path::new("/state/.shellio.sock.77")
    );
}
