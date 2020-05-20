/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use net::PeerAddress;
use net::Neighbor;
use net::NeighborKey;
use net::Error as net_error;

use util::db::Error as db_error;
use util::db::DBConn;

use std::net;
use std::net::SocketAddr;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::io;
use std::io::Read;
use std::io::Write;
use std::io::Error as io_error;
use std::io::ErrorKind;
use std::time;

use util::log;
use util::sleep_ms;

use mio;
use mio::net as mio_net;
use mio::Ready;
use mio::Token;
use mio::PollOpt;

use std::net::Shutdown;

use rand::RngCore;
use rand;

pub const NUM_NEIGHBORS : u32 = 32;

const SERVER : Token = mio::Token(0);

pub struct NetworkPollState {
    pub new: HashMap<usize, mio_net::TcpStream>,
    pub ready: Vec<usize>
}

impl NetworkPollState {
    pub fn new() -> NetworkPollState {
        NetworkPollState {
            new: HashMap::new(),
            ready: vec![]
        }
    }
}

// state for a single network server 
pub struct NetworkServerState {
    addr: SocketAddr,
    server_socket: mio_net::TcpListener,
    server_event: mio::Token,
}

// state for the entire network
pub struct NetworkState {
    poll: mio::Poll,
    events: mio::Events,
    event_capacity: usize,
    servers: Vec<NetworkServerState>,
    count: usize,
    event_map: HashMap<usize, usize>        // map socket events to their registered server socket (including server sockets)
}

impl NetworkState {
    pub fn new(event_capacity: usize) -> Result<NetworkState, net_error> {
        let poll = mio::Poll::new()
            .map_err(|e| {
                error!("Failed to initialize poller: {:?}", e);
                net_error::BindError
            })?;
        
        let events = mio::Events::with_capacity(event_capacity);

        Ok(NetworkState {
            poll: poll,
            events: events,
            event_capacity: event_capacity,
            servers: vec![],
            count: 1,
            event_map: HashMap::new()
        })
    }

    fn bind_address(addr: &SocketAddr) -> Result<mio_net::TcpListener, net_error> {
        if !cfg!(test) {
            mio_net::TcpListener::bind(addr)
                .map_err(|e| {
                    error!("Failed to bind to {:?}: {:?}", addr, e);
                    net_error::BindError
                })
        }
        else {
            let mut backoff = 1000;
            let mut rng = rand::thread_rng();
            let mut count = 1000;
            loop {
                match mio_net::TcpListener::bind(addr) {
                    Ok(server) => {
                        return Ok(server);
                    },
                    Err(e) => match e.kind() {
                        io::ErrorKind::AddrInUse => {
                            debug!("Waiting {} millis and trying to bind {:?} again", backoff, addr);
                            sleep_ms(backoff);
                            backoff = count + (rng.next_u64() % count);
                            count += count;
                            continue;
                        },
                        _ => {
                            debug!("Failed to bind {:?}: {:?}", addr, &e);
                            return Err(net_error::BindError);
                        }
                    }
                }
            }
        }
    }

    /// Bind to the given socket address.
    /// Returns the handle to the poll state, used to key network poll events.
    pub fn bind(&mut self, addr: &SocketAddr) -> Result<usize, net_error> {
        let server = NetworkState::bind_address(addr)?;
        let next_server_event = self.next_event_id()?;

        self.poll.register(&server, mio::Token(next_server_event), Ready::all(), PollOpt::edge())
            .map_err(|e| {
                error!("Failed to register server socket: {:?}", &e);
                net_error::BindError
            })?;

        let network_server = NetworkServerState {
            addr: addr.clone(),
            server_socket: server,
            server_event: mio::Token(next_server_event),
        };

        assert!(!self.event_map.contains_key(&next_server_event), "BUG: failed to generate an unused server event ID");

        self.servers.push(network_server);
        self.event_map.insert(next_server_event, 0);        // server events always mapped to 0

        Ok(next_server_event)
    }

    /// Register a socket for read/write notifications with this poller.
    /// Try to use the given hint_event_id value, but generate a different event ID if it's been
    /// taken.
    /// Return the actual event ID used (it may be different than hint_event_id)
    pub fn register(&mut self, server_event_id: usize, hint_event_id: usize, sock: &mio_net::TcpStream) -> Result<usize, net_error> {
        let hint_event_id = hint_event_id % self.event_capacity;
        if let Some(x) = self.event_map.get(&server_event_id) {
            if x != &0 {
                // not a server event
                error!("Server event ID {} not mapped to a server token, but to {}", &server_event_id, x);
                return Err(net_error::RegisterError);
            }
        }
        else {
            // not a server event
            panic!("Not a server event ID: {}", &server_event_id);
        }

        // if the event ID is in use, then find another one
        let event_id = 
            if self.event_map.contains_key(&hint_event_id) {
                self.next_event_id()?
            }
            else {
                hint_event_id
            };
        
        self.poll.register(sock, mio::Token(event_id), Ready::all(), PollOpt::edge())
            .map_err(|e| {
                error!("Failed to register socket on server {} event ID {} ({}): {:?}", server_event_id, event_id, hint_event_id, &e);
                net_error::RegisterError
            })?;

        self.event_map.insert(event_id, server_event_id);
        test_debug!("Register socket {:?} as event {} ({}) on server {}", sock, event_id, hint_event_id, server_event_id);
        Ok(event_id)
    }

    /// Deregister a socket event
    pub fn deregister(&mut self, event_id: usize, sock: &mio_net::TcpStream) -> Result<(), net_error> {
        self.poll.deregister(sock)
            .map_err(|e| {
                error!("Failed to deregister socket {}: {:?}", event_id, &e);
                net_error::RegisterError
            })?;
        
        self.event_map.remove(&event_id);

        sock.shutdown(Shutdown::Both)
            .map_err(|_e| net_error::SocketError)?;

        test_debug!("Socket deregistered: {}, {:?}", event_id, sock);
        Ok(())
    }

    fn make_next_event_id(&self, cur_count: usize, in_use: &HashSet<usize>) -> Result<usize, net_error> {
        let mut ret = cur_count;
        for _ in 0..self.event_capacity {
            if self.event_map.contains_key(&ret) || in_use.contains(&ret) {
                ret = (ret + 1) % self.event_capacity;
            }
            else {
                return Ok(ret);
            }
        }
        return Err(net_error::TooManyPeers);
    }

    /// next event ID
    pub fn next_event_id(&mut self) -> Result<usize, net_error> {
        let ret = self.make_next_event_id(self.count, &HashSet::new())?;
        self.count = (ret + 1) % self.event_capacity;
        Ok(ret)
    }

    /// Connect to a remote peer, but don't register it with the poll handle.
    /// The underlying connect(2) is _asynchronous_, so the caller will need to register it with a
    /// poll handle and wait for it to be connected.
    pub fn connect(addr: &SocketAddr) -> Result<mio_net::TcpStream, net_error> {
        let stream = mio_net::TcpStream::connect(addr)
            .map_err(|_e| {
                test_debug!("Failed to convert to mio stream: {:?}", &_e);
                net_error::ConnectionError
            })?;

        // set some helpful defaults
        // Don't go crazy on TIME_WAIT states; have them all die after 5 seconds
        stream.set_linger(Some(time::Duration::from_millis(5000)))
            .map_err(|_e| {
                test_debug!("Failed to set SO_LINGER: {:?}", &_e);
                net_error::ConnectionError
            })?;

        // Disable Nagle algorithm
        stream.set_nodelay(true)
            .map_err(|_e| {
                test_debug!("Failed to set TCP_NODELAY: {:?}", &_e);
                net_error::ConnectionError
            })?;

        // Make sure keep-alive is on, since at least in p2p messages, we keep sockets around
        // for a while.  Linux default is 7200 seconds, so make sure we keep it here.
        stream.set_keepalive(Some(time::Duration::from_millis(7200 * 1000)))
            .map_err(|_e| {
                test_debug!("Failed to set TCP_KEEPALIVE and/or SO_KEEPALIVE: {:?}", &_e);
                net_error::ConnectionError
            })?;

        if cfg!(test) {
            // edge-trigger torture test
            stream.set_send_buffer_size(32).unwrap();
            stream.set_recv_buffer_size(32).unwrap();
        }

        test_debug!("New socket connected to {:?}: {:?}", addr, &stream);
        Ok(stream)
    }

    /// Poll all server sockets.
    /// Returns a map between network server handles (returned by bind()) and their new polling state
    pub fn poll(&mut self, timeout: u64) -> Result<HashMap<usize, NetworkPollState>, net_error> {
        self.events.clear();
        self.poll.poll(&mut self.events, Some(Duration::from_millis(timeout)))
            .map_err(|e| {
                error!("Failed to poll: {:?}", &e);
                net_error::PollError
            })?;

        let mut poll_states = HashMap::new();
        for server in self.servers.iter() {
            // pre-populate with server tokens
            let server_event_id = usize::from(server.server_event);
            poll_states.insert(server_event_id, NetworkPollState::new());
        }

        let mut new_events = HashSet::new();
       
        for event in &self.events {
            let token = event.token();
            let mut is_server_event = false;

            for server in self.servers.iter() {
                // server token?
                if token == server.server_event {
                    // new inbound connection(s)
                    is_server_event = true;
                    let poll_state = poll_states.get_mut(&usize::from(token)).expect(&format!("BUG: FATAL: no poll state registered for server {}", usize::from(token)));
                    
                    loop {
                        let (client_sock, _client_addr) = match server.server_socket.accept() {
                            Ok((client_sock, client_addr)) => (client_sock, client_addr),
                            Err(e) => {
                                match e.kind() {
                                    ErrorKind::WouldBlock => {
                                        break;
                                    },
                                    _ => {
                                        return Err(net_error::AcceptError);
                                    }
                                }
                            }
                        };

                        // this does the same thing as next_event_id(), but we can't borrow self
                        // mutably here (so we'll just do the increment-mod directly).
                        let next_event_id = match self.make_next_event_id(self.count, &new_events) {
                            Ok(eid) => eid,
                            Err(_e) => {
                                // no poll slots available. Close the socket and carry on.
                                info!("Too many peers, closing {:?}", &_client_addr);
                                let _ = client_sock.shutdown(Shutdown::Both);
                                continue;
                            }
                        };

                        self.count = (next_event_id + 1) % self.event_capacity;

                        new_events.insert(next_event_id);
                        
                        test_debug!("New socket accepted from {:?} (event {}) on server {:?}: {:?}", &_client_addr, next_event_id, &server.server_socket, &client_sock);
                        poll_state.new.insert(next_event_id, client_sock);
                    }

                    break;
                }
            }

            if is_server_event {
                continue;
            }

            // event for a client of one of our servers.  which one?
            let event_id = usize::from(token);
            match self.event_map.get(&event_id) {
                Some(server_event_id) => {
                    if let Some(poll_state) = poll_states.get_mut(server_event_id) {
                        test_debug!("Wakeup socket event {} on server {}", event_id, server_event_id);
                        poll_state.ready.push(event_id);
                    }
                    else {
                        warn!("Unknown server event ID {}", server_event_id);
                    }
                },
                None => {
                    warn!("Surreptitious readiness event {}", event_id);
                }
            }
        }

        Ok(poll_states)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use mio;
    use mio::net as mio_net;
    use mio::Ready;
    use mio::Token;
    use mio::PollOpt;
    
    use std::collections::HashSet;

    #[test]
    fn test_bind() {
        let mut ns = NetworkState::new(100).unwrap();
        let mut server_events = HashSet::new();
        for port in 49000..49010 {
            let addr = format!("127.0.0.1:{}", &port).parse::<SocketAddr>().unwrap();
            let event_id = ns.bind(&addr).unwrap();
            assert!(!server_events.contains(&event_id));
            server_events.insert(event_id);
        }
    }

    #[test]
    fn test_register_deregister() {
        let mut ns = NetworkState::new(100).unwrap();
        let mut server_events = vec![];
        let mut event_ids = HashSet::new();
        for port in 49010..49020 {
            let addr = format!("127.0.0.1:{}", &port).parse::<SocketAddr>().unwrap();
            let event_id = ns.bind(&addr).unwrap();
            server_events.push(event_id);
            event_ids.insert(event_id);
        }

        let mut client_events = vec![];
        for port in 49010..49020 {
            let addr = format!("127.0.0.1:{}", &port).parse::<SocketAddr>().unwrap();
            let sock = NetworkState::connect(&addr).unwrap();

            let event_id = ns.register(server_events[port - 49010], 1, &sock).unwrap();
            assert!(event_id != 0);
            assert!(!event_ids.contains(&event_id));
            ns.deregister(event_id, &sock).unwrap();

            let event_id = ns.register(server_events[port - 49010], 101, &sock).unwrap();
            assert!(event_id != 0);
            assert!(!event_ids.contains(&event_id));
            ns.deregister(event_id, &sock).unwrap();
            
            let event_id = ns.register(server_events[port - 49010], server_events[port - 49010], &sock).unwrap();
            assert!(event_id != 0);
            assert!(!event_ids.contains(&event_id));
            ns.deregister(event_id, &sock).unwrap();

            let event_id = ns.register(server_events[port - 49010], 11, &sock).unwrap();
            assert!(!event_ids.contains(&event_id));

            event_ids.insert(event_id);
            client_events.push(event_id);
        }

        test_debug!("=====");
        for port in 49010..49020 {
            let addr = format!("127.0.0.1:{}", &port).parse::<SocketAddr>().unwrap();
            let sock = NetworkState::connect(&addr).unwrap();

            // can't use non-server events
            assert_eq!(Err(net_error::RegisterError), ns.register(client_events[port - 49010], port - 49010 + 1, &sock));
        }
    }

    #[test]
    fn test_register_too_many_peers() {
        let mut ns = NetworkState::new(20).unwrap();
        let mut server_events = vec![];
        let mut event_ids = HashSet::new();
        for port in 49020..49030 {
            let addr = format!("127.0.0.1:{}", &port).parse::<SocketAddr>().unwrap();
            let event_id = ns.bind(&addr).unwrap();
            server_events.push(event_id);
            event_ids.insert(event_id);
            
            let sock = NetworkState::connect(&addr).unwrap();

            // register 10 client events
            let event_id = ns.register(server_events[port - 49020], 11, &sock).unwrap();
            assert!(!event_ids.contains(&event_id));
        }

        // the 21st socket should fail
        let addr = "127.0.0.1:49020".parse::<SocketAddr>().unwrap();
        let sock = NetworkState::connect(&addr).unwrap();
        let res = ns.register(server_events[0], 11, &sock);
        assert_eq!(Err(net_error::TooManyPeers), res);
    }
}
