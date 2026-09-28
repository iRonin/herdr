//! The process facts a fast relaunch is judged by: who sent a request, whether a
//! process group still has members, and a process's parent. Each test uses real
//! processes, so a helper that answered from the wrong process fails here.

use super::{local_peer_process, process_group_exists, process_parent_id, PeerProcess};
use std::os::unix::process::CommandExt as _;

fn spawn_group_leader() -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .expect("spawn sleep")
}

#[test]
fn local_peer_process_names_the_connecting_process_not_the_server() {
    let path = std::env::temp_dir().join(format!("herdr-peer-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let listener = crate::ipc::bind_local_listener(&path).unwrap();

    let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    }
    assert!(c_path.as_bytes_with_nul().len() <= addr.sun_path.len());
    for (slot, byte) in addr.sun_path.iter_mut().zip(c_path.as_bytes_with_nul()) {
        *slot = *byte as libc::c_char;
    }
    // A child in its own process group connects and waits to be killed. It only makes
    // async-signal-safe calls, as a child forked from a threaded test process must.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::setpgid(0, 0);
            let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
            libc::connect(
                fd,
                &addr as *const libc::sockaddr_un as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
            );
            loop {
                libc::pause();
            }
        }
    }

    use interprocess::local_socket::{traits::Listener as _, ListenerNonblockingMode};
    listener
        .set_nonblocking(ListenerNonblockingMode::Accept)
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let accepted = loop {
        match listener.accept() {
            Ok(stream) => break Some(stream),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() > deadline {
                    break None;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(err) => panic!("accept failed: {err}"),
        }
    };
    let peer = accepted.as_ref().and_then(local_peer_process);
    unsafe {
        libc::kill(child, libc::SIGKILL);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    drop(accepted);
    drop(listener);
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        peer,
        Some(PeerProcess {
            pid: child as u32,
            process_group: child as u32,
        }),
        "the sender is the child that connected, in its own group (this test is pid {})",
        std::process::id()
    );
}

#[test]
fn process_group_exists_until_its_last_member_is_gone() {
    let mut child = spawn_group_leader();
    let group = child.id();
    assert!(process_group_exists(group), "a running group leader");
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        !process_group_exists(group),
        "the group is empty once its leader is reaped"
    );
}

#[test]
fn process_group_exists_fails_safe_on_groups_it_must_not_signal() {
    // kill(0, 0) and kill(-1, 0) address the caller's group and every process.
    assert!(process_group_exists(0));
    assert!(process_group_exists(1));
}

#[test]
fn process_parent_id_names_the_spawning_process() {
    let mut child = spawn_group_leader();
    let parent = process_parent_id(child.id());
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(parent, Some(std::process::id()));
}
