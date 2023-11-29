#![feature(array_chunks)]

use std::collections::VecDeque;
use std::io::IoSlice;
use std::mem;
use std::{path::PathBuf, io::IoSliceMut};
use std::process::{Command, exit};
use std::str::FromStr;

use rustix::fs::unlink;
use rustix::net::{RecvFlags, RecvAncillaryBuffer, sendmsg, SendAncillaryMessage, SendFlags, RecvAncillaryMessage, listen, accept_with};
use rustix::{event::{poll, PollFd, PollFlags}, fd::{OwnedFd, AsFd}, net::{socket_with, AddressFamily, SocketType, SocketFlags, bind_unix, SocketAddrUnix, connect_unix, SendAncillaryBuffer, recvmsg}, io::Errno};

fn main() {
    let wayland = std::env::var("WAYLAND_DISPLAY").expect("WAYLAND_DISPLAY not set");
    let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR").expect("XDG_RUNTIME_DIR not set");

    let pid = std::process::id();
    let wayland_wrap = format!("wayland-wrap-{}", pid);

    let wayland_path = if wayland.starts_with('/') {
        PathBuf::from(wayland)
    } else {
        [&xdg_runtime_dir, &wayland].iter().collect()
    };
    let parent_sock_addr = SocketAddrUnix::new(wayland_path).expect("invalid bind addr for parent");


    let server_socket = socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK, None).expect("failed to open unix socket");


    let mut sock_path = PathBuf::from_str(&xdg_runtime_dir).unwrap();
    sock_path.push(&wayland_wrap);
    let sock_addr = SocketAddrUnix::new(&sock_path).expect("invalid bind addr");
    bind_unix(&server_socket, &sock_addr).expect("failed to bind unix socket");
    listen(&server_socket, 128).expect("failed to set server socket to listen mode");


    let args: Vec<String> = std::env::args().collect();

    let mut child = Command::new(&args[1])
        .args(&args[2..])
        .env("WAYLAND_DISPLAY", wayland_wrap)
        .spawn()
        .expect("failed to execute child");

    
    struct ProxiedConnection {
        parent: Option<OwnedFd>,
        child: Option<OwnedFd>,
        parent_connected: bool,
        to_parent: VecDeque<BufferedMessage>,
        to_child: VecDeque<BufferedMessage>,
    }


    let mut connections: Vec<ProxiedConnection> = Vec::new();

    loop {
        let mut poll_fds = Vec::with_capacity(1 + connections.len());
        
        poll_fds.extend(connections.iter().flat_map(|conn| {
            let mut parent_flags = PollFlags::IN;
            let mut child_flags = PollFlags::IN;
            if !conn.parent_connected || !conn.to_parent.is_empty() {
                parent_flags |= PollFlags::OUT
            }
            if !conn.to_child.is_empty() {
                child_flags |= PollFlags::OUT
            }

            [
                PollFd::from_borrowed_fd(conn.parent.as_ref().unwrap().as_fd(), parent_flags),
                PollFd::from_borrowed_fd(conn.child.as_ref().unwrap().as_fd(), child_flags)
            ]
        }));

        poll_fds.push(PollFd::new(&server_socket, PollFlags::IN));

        // wait 30 seconds, if we then have no connections and no children at that point we exit
        match poll(poll_fds.as_mut(), 30000) {
            Ok(_) => {},
            Err(e) if e == Errno::INTR => continue,
            Err(e) => panic!("unexpected poll() error {}", e.kind())
        }

        let mut poll_flags: Vec<_> = poll_fds.into_iter().map(|p| p.revents()).collect();

        let server_flags = poll_flags.pop().unwrap();

        if server_flags.contains(PollFlags::IN) {
            loop {
                match accept_with(&server_socket, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK) {
                    Ok(child) => {
                        let parent = socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC | SocketFlags::NONBLOCK, None).expect("failed to open unix socket");
                        let parent_connected = match connect_unix(&parent, &parent_sock_addr) {
                            Ok(_) => true,
                            Err(e) if e == Errno::AGAIN => false,
                            Err(e) => panic!("unexpected error on connect() {}", e),
                        };
                        connections.push(ProxiedConnection { parent: Some(parent), child: Some(child), parent_connected, to_parent: VecDeque::new(), to_child: VecDeque::new()});
                    }
                    Err(e) if e == Errno::AGAIN => break,
                    Err(e) => panic!("unexpected error during accept() {}", e)
                }
            }
        }

        for ([parent_flags, child_flags], conn) in poll_flags.array_chunks().zip(connections.iter_mut()) {
            if parent_flags.intersects(PollFlags::HUP | PollFlags::ERR) || child_flags.intersects(PollFlags::HUP | PollFlags::ERR) {
                // poll indicates error. close.
                conn.child.take();
                conn.parent.take();
                continue;
            }

            if !conn.parent_connected && parent_flags.contains(PollFlags::OUT) {
                conn.parent_connected = true
            }
            if conn.parent_connected {
                transfer_or_queue(&mut conn.parent, parent_flags, &mut conn.child, &mut conn.to_child);
                transfer_or_queue(&mut conn.child, child_flags, &mut conn.parent, &mut conn.to_parent);
                drain_queue(&mut conn.parent, &parent_flags, &mut conn.to_parent);
                drain_queue(&mut conn.child, &child_flags, &mut conn.to_child);
            }
        }

        // drop closed connections
        connections.retain(|c| c.child.is_some() && c.parent.is_some());


        match (child.try_wait(), connections.len()) {
            (Ok(Some(_)), 0) => {
                drop(server_socket);
                unlink(sock_path).expect("failed to unlink socket");
                eprint!("child exited and no open connections, exiting");
                exit(0);
            }
            _ => {}
        }
    }
}


fn transfer_or_queue(from: &mut Option<OwnedFd>, from_flags: &PollFlags, to: &mut Option<OwnedFd>, queued: &mut VecDeque<BufferedMessage>) {
    if !from_flags.contains(PollFlags::IN) {
        return;
    }

    let mut bytes = [0u8; 1024];
    // this is the max per sendmsg
    let mut space = [0; rustix::cmsg_space!(ScmRights(253))];

    // assumption: we don't receive FDs too often so this won't actually get allocated
    let mut fds: Vec<OwnedFd> = Vec::new();


    loop {
        fds.clear();
        let mut recv_cmsg = RecvAncillaryBuffer::new(&mut space);

        if from.is_none() || to.is_none() {
            return
        }

        match recvmsg(from.as_ref().expect("Some(from fd)"), &mut [IoSliceMut::new(&mut bytes)], &mut recv_cmsg, RecvFlags::CMSG_CLOEXEC) {
            Err(e) if e == Errno::CONNRESET => {
                from.take();
                to.take();
                return
            }
            Err(e) if e == Errno::WOULDBLOCK || e == Errno::AGAIN => return,
            Err(e) => panic!("unexpected error on recv {}", e.kind()),
            Ok(recv) => {
                if recv.bytes == 0 {
                    // EOF, close connections.
                    // TODO: this is kinda dirty, we could shutdown more gracefully by draining messages that are still buffered if the sending side is still open
                    from.take();
                    to.take();
                    return
                }
    
                let bytes = &bytes[0..recv.bytes];
                recv_cmsg.drain().for_each(|msg| {
                    match msg {
                        RecvAncillaryMessage::ScmRights(rights) => fds.extend(rights),
                        _ => {},
                    }
                });
                drop(recv_cmsg);
    
                // attempt direct resend, queue otherwise

                space.fill(0);
                let mut send_cmsg = SendAncillaryBuffer::new(&mut space);
                let to_send: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
                send_cmsg.push(SendAncillaryMessage::ScmRights(to_send.as_slice()));
    
    
                match sendmsg(to.as_ref().expect("Some(to fd)"), &[IoSlice::new(&bytes)], &mut send_cmsg, SendFlags::empty()) {
                    Ok(0) | Err(Errno::CONNRESET) => {
                        from.take();
                        to.take();
                        return
                    }
                    Ok(sent) => {
                        if sent != bytes.len() {
                            queued.push_back(BufferedMessage { fds: Vec::new(), bytes: bytes[sent..].iter().copied().collect()})
                        }
                    }
                    Err(e) if e == Errno::WOULDBLOCK || e == Errno::AGAIN => {
                        queued.push_back(BufferedMessage { fds: mem::take(&mut fds), bytes: bytes.iter().copied().collect() });
                        return
                    },
                    Err(e) => panic!("unexpected error in sendmsg() {}", e)
                }
            }
        }
    }
}


fn drain_queue(to: &mut Option<OwnedFd>, to_flags: &PollFlags, queued: &mut VecDeque<BufferedMessage>) {
    if !to_flags.contains(PollFlags::OUT) {
        return;
    }

    loop {
        if to.is_none() {
            return;
        }

        let Some(BufferedMessage {fds, mut bytes}) = queued.pop_front() else {
            return;
        };

        let mut space = [0; rustix::cmsg_space!(ScmRights(253))];
        let mut send_cmsg = SendAncillaryBuffer::new(&mut space);

        let to_send: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
        send_cmsg.push(SendAncillaryMessage::ScmRights(to_send.as_slice()));

        let (front, back) = bytes.as_slices();

        match sendmsg(to.as_ref().expect("Some(to fd)"), &[IoSlice::new(&front), IoSlice::new(&back)], &mut send_cmsg, SendFlags::empty()) {
            Ok(0) | Err(Errno::CONNRESET) => {
                to.take();
                return
            }
            Ok(sent) => {
                if sent != bytes.len() {
                    bytes.drain(..sent);
                    queued.push_back(BufferedMessage { fds: Vec::new(), bytes})
                }
            }
            Err(e) if e == Errno::WOULDBLOCK || e == Errno::AGAIN => {
                queued.push_front(BufferedMessage { fds, bytes });
                return
            },
            Err(e) => panic!("unexpected error in sendmsg() {}", e)
        }
    }
}

struct BufferedMessage {
    fds: Vec<OwnedFd>,
    bytes: VecDeque<u8>
}
