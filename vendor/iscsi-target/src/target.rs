//! iSCSI target server implementation
//!
//! This module provides the main server structure, TCP listener, and connection handling.

use crate::error::{IscsiError, ScsiResult};
use crate::pdu::{self, IscsiPdu, BHS_SIZE, opcode, flags, scsi_status, serialize_text_parameters};
use crate::scsi::{ScsiBlockDevice, ScsiHandler, ScsiResponse};
use crate::typestate_session::{AnySession, SessionData};
use crate::session::PendingWrite;
use crate::pdu::ScsiCommandPdu;
use byteorder::{BigEndian, ByteOrder};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, Shutdown};
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::thread;
use std::time::Duration;

/// Default iSCSI port
pub const ISCSI_PORT: u16 = 3260;

/// iSCSI target server
pub struct IscsiTarget<D: ScsiBlockDevice> {
    bind_addr: String,
    /// Optional host:port returned in SendTargets (defaults to socket local_addr).
    advertise_addr: Option<String>,
    target_name: String,
    target_alias: String,
    device: Arc<Mutex<D>>,
    running: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    auth_config: crate::auth::AuthConfig,
    max_connections: u32,
    active_connections: Arc<std::sync::atomic::AtomicUsize>,
    max_sessions: u32,
    active_sessions: Arc<std::sync::atomic::AtomicUsize>,
    allowed_initiators: Option<Vec<String>>,
}

impl<D: ScsiBlockDevice + Send + 'static> IscsiTarget<D> {
    /// Create a new builder for configuring the target
    pub fn builder() -> IscsiTargetBuilder<D> {
        IscsiTargetBuilder::new()
    }

    /// Run the iSCSI target server
    ///
    /// This blocks the current thread and processes incoming connections.
    pub fn run(&self) -> ScsiResult<()> {
        log::info!("iSCSI target starting on {}", self.bind_addr);
        log::info!("Target name: {}", self.target_name);

        let listener = TcpListener::bind(&self.bind_addr)
            .map_err(IscsiError::Io)?;

        // Set non-blocking for graceful shutdown checking
        listener.set_nonblocking(true)
            .map_err(IscsiError::Io)?;

        self.running.store(true, Ordering::SeqCst);

        log::info!("iSCSI target listening on {}", self.bind_addr);

        while self.running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    log::info!("New connection from {}", addr);

                    // Check connection limit
                    let current = self.active_connections.fetch_add(1, Ordering::SeqCst);
                    if current >= self.max_connections as usize {
                        log::warn!("Connection rejected from {}: too many connections ({}/{})",
                            addr, current + 1, self.max_connections);
                        self.active_connections.fetch_sub(1, Ordering::SeqCst);

                        // Send TOO_MANY_CONNECTIONS reject and close
                        let _ = send_connection_limit_reject(stream);
                        continue;
                    }

                    log::debug!("Accepted connection from {} ({}/{} active)",
                        addr, current + 1, self.max_connections);

                    let device = Arc::clone(&self.device);
                    let target_name = self.target_name.clone();
                    let target_alias = self.target_alias.clone();
                    let advertise_addr = self.advertise_addr.clone();
                    let auth_config = self.auth_config.clone();
                    let running = Arc::clone(&self.running);
                    let shutting_down = Arc::clone(&self.shutting_down);
                    let active_connections = Arc::clone(&self.active_connections);
                    let max_sessions = self.max_sessions;
                    let active_sessions = Arc::clone(&self.active_sessions);
                    let allowed_initiators = self.allowed_initiators.clone();

                    thread::spawn(move || {
                        let session_entered = match handle_connection(
                            stream,
                            device,
                            &target_name,
                            &target_alias,
                            advertise_addr.as_deref(),
                            auth_config,
                            running,
                            shutting_down,
                            max_sessions,
                            Arc::clone(&active_sessions),
                            allowed_initiators,
                            addr,
                        ) {
                            Ok(entered) => entered,
                            Err(e) => {
                                log::error!("Connection from {} failed: {}", addr, e);
                                false
                            }
                        };

                        log::info!("Connection closed from {} (full_feature={})", addr, session_entered);

                        // Decrement connection count
                        let prev = active_connections.fetch_sub(1, Ordering::SeqCst);
                        log::debug!("Connection count: {} -> {}", prev, prev - 1);

                        // Decrement session count if a session was established
                        if session_entered {
                            let prev = active_sessions.fetch_sub(1, Ordering::SeqCst);
                            log::debug!("Session count: {} -> {}", prev, prev - 1);
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // No connection available, sleep briefly and retry
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    log::error!("Accept error: {}", e);
                }
            }
        }

        log::info!("iSCSI target shutting down");
        Ok(())
    }

    /// Get the current number of active connections
    pub fn active_connection_count(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }

    /// Get the current number of active sessions
    pub fn active_session_count(&self) -> usize {
        self.active_sessions.load(Ordering::SeqCst)
    }

    /// Initiate graceful shutdown - reject new logins but allow existing sessions to complete
    pub fn shutdown_gracefully(&self) {
        log::info!("Initiating graceful shutdown - new logins will be rejected");
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Signal the server to stop immediately
    pub fn stop(&self) {
        log::info!("Stopping iSCSI target server");
        self.running.store(false, Ordering::SeqCst);
    }

    /// Check if the server is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Check if the server is in graceful shutdown mode
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

/// Send TOO_MANY_CONNECTIONS reject to a new connection
fn send_connection_limit_reject(mut stream: TcpStream) -> ScsiResult<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let mut bhs = [0u8; 48];
    if stream.read_exact(&mut bhs).is_ok() {
        let itt = u32::from_be_bytes([bhs[16], bhs[17], bhs[18], bhs[19]]);
        let data = SessionData::default();
        let reject_pdu = data.create_login_reject(itt, pdu::login_status::INITIATOR_ERROR, 0x06);
        let _ = write_pdu(&mut stream, &reject_pdu);
    }

    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

/// Handle a single iSCSI connection using typestate session
fn handle_connection<D: ScsiBlockDevice>(
    mut stream: TcpStream,
    device: Arc<Mutex<D>>,
    target_name: &str,
    target_alias: &str,
    advertise_addr: Option<&str>,
    auth_config: crate::auth::AuthConfig,
    running: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    max_sessions: u32,
    active_sessions: Arc<std::sync::atomic::AtomicUsize>,
    allowed_initiators: Option<Vec<String>>,
    peer: std::net::SocketAddr,
) -> ScsiResult<bool> {
    let local_addr = stream.local_addr().map_err(IscsiError::Io)?;
    stream.set_nonblocking(false).map_err(IscsiError::Io)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(IscsiError::Io)?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(IscsiError::Io)?;

    // Create session using typestate pattern with AnySession wrapper
    let mut session = AnySession::new_configured(
        auth_config,
        target_name,
        target_alias,
        allowed_initiators,
    );

    let mut session_entered = false;

    // Address for discovery responses. Prefer an explicit advertise address
    // (needed behind Docker/NAT); otherwise use the socket local_addr, which
    // resolves to the actual interface IP even when bound to 0.0.0.0.
    let target_address = advertise_addr
        .map(|s| s.to_string())
        .unwrap_or_else(|| local_addr.to_string());
    log::info!(
        "Handling connection from {} (local={}, advertise={}, target={})",
        peer, local_addr, target_address, target_name
    );

    // Clone stream for writer — reader thread handles NOP-Out directly
    let mut write_stream = stream.try_clone().map_err(IscsiError::Io)?;
    let nop_write_stream = std::sync::Arc::new(std::sync::Mutex::new(
        stream.try_clone().map_err(IscsiError::Io)?
    ));

    // Shared digest flags — set after login negotiation
    let use_header_digest = Arc::new(AtomicBool::new(false));
    let use_data_digest = Arc::new(AtomicBool::new(false));
    let reader_hd = use_header_digest.clone();
    let reader_dd = use_data_digest.clone();
    let nop_hd = use_header_digest.clone();
    let nop_dd = use_data_digest.clone();

    // Channel for PDUs from reader thread to main processing loop
    let (pdu_tx, pdu_rx) = std::sync::mpsc::channel::<IscsiPdu>();
    // Per-connection flag — don't use the global `running` to stop the reader,
    // or closing one connection kills the whole target.
    let conn_running = Arc::new(AtomicBool::new(true));
    let reader_running = conn_running.clone();
    let nop_writer = nop_write_stream.clone();

    // Reader thread: reads PDUs, handles NOP-Out inline, forwards rest
    let reader_handle = std::thread::spawn(move || {
        while reader_running.load(Ordering::SeqCst) {
            // Pass atomic refs so flags are loaded AFTER the blocking BHS read,
            // not before — avoids stale state on the first post-login PDU.
            let pdu = match read_pdu_atomic(&mut stream, &reader_hd, &reader_dd) {
                Ok(pdu) => pdu,
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::info!("Peer {} closed connection (EOF)", peer);
                    break;
                }
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    continue; // Don't close on timeout — just keep reading
                }
                Err(e) => {
                    log::error!("Error reading PDU from {}: {}", peer, e);
                    break;
                }
            };

            // Handle NOP-Out directly in reader thread for fast response
            if pdu.opcode == opcode::NOP_OUT {
                log::debug!("NOP-Out received, responding inline");
                // Build minimal NOP-In response
                let mut resp = IscsiPdu::new();
                resp.opcode = 0x20; // NOP-In
                resp.flags = 0x80; // Final
                resp.itt = pdu.itt;
                resp.specific[0..4].copy_from_slice(&pdu.specific[0..4]); // TTT
                // StatSN, ExpCmdSN, MaxCmdSN will be approximate but good enough for keepalive
                if let Ok(mut writer) = nop_writer.lock() {
                    let _ = write_pdu_digest(&mut *writer, &resp, nop_hd.load(Ordering::SeqCst), nop_dd.load(Ordering::SeqCst));
                }
                continue;
            }

            if pdu_tx.send(pdu).is_err() {
                break; // Main thread exited
            }
        }
    });

    // Main connection loop — processes PDUs from reader thread
    while running.load(Ordering::SeqCst) {
        let pdu = match pdu_rx.recv_timeout(Duration::from_secs(300)) {
            Ok(pdu) => pdu,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                log::info!("Connection from {} idle timeout (300s), closing", peer);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                log::info!(
                    "Connection from {} reader exited (state={}, full_feature={})",
                    peer,
                    session.state_name(),
                    session_entered
                );
                break;
            }
        };

        let rx_level_info = !session.is_full_feature()
            || !matches!(
                pdu.opcode,
                opcode::SCSI_COMMAND | opcode::SCSI_DATA_OUT | opcode::NOP_OUT
            );
        if rx_level_info {
            log::info!(
                "RX {} from {} (opcode 0x{:02x}, state={}, data_len={})",
                pdu.opcode_name(),
                peer,
                pdu.opcode,
                session.state_name(),
                pdu.data.len()
            );
        } else {
            log::debug!(
                "RX {} from {} (opcode 0x{:02x}, state={}, data_len={})",
                pdu.opcode_name(),
                peer,
                pdu.opcode,
                session.state_name(),
                pdu.data.len()
            );
        }

        let was_full_feature = session.is_full_feature();

        // Process PDU based on session state
        let responses = if session.is_login_phase() {
            handle_login_phase(&mut session, &pdu, target_name, &target_address, &shutting_down, max_sessions, &active_sessions)?
        } else if session.is_full_feature() {
            handle_full_feature_phase(&mut session, &pdu, &device, target_name, &target_address)?
        } else {
            // Session ended (Logout or Failed)
            log::info!("Session from {} ended (state: {})", peer, session.state_name());
            break;
        };

        // Detect transition to FullFeaturePhase (but don't enable digests yet —
        // the login response itself must be sent without digests)
        let entering_full_feature = !was_full_feature && session.is_full_feature();

        // Send responses (login response goes out without digests)
        for resp_pdu in responses {
            let tx_info = !session.is_full_feature()
                || entering_full_feature
                || !matches!(
                    resp_pdu.opcode,
                    opcode::SCSI_DATA_IN | opcode::SCSI_RESPONSE | opcode::R2T | opcode::NOP_IN
                );
            if tx_info {
                log::info!(
                    "TX {} to {} (opcode 0x{:02x}, data_len={})",
                    resp_pdu.opcode_name(),
                    peer,
                    resp_pdu.opcode,
                    resp_pdu.data.len()
                );
            } else {
                log::debug!(
                    "TX {} to {} (opcode 0x{:02x}, data_len={})",
                    resp_pdu.opcode_name(),
                    peer,
                    resp_pdu.opcode,
                    resp_pdu.data.len()
                );
            }
            write_pdu_digest(&mut write_stream, &resp_pdu, use_header_digest.load(Ordering::SeqCst), use_data_digest.load(Ordering::SeqCst))?;
        }

        // NOW enable digests — subsequent PDUs will use them
        if entering_full_feature {
            if let Some(data) = session.data() {
                let hd = matches!(data.params.header_digest, crate::session::DigestType::CRC32C);
                let dd = matches!(data.params.data_digest, crate::session::DigestType::CRC32C);
                use_header_digest.store(hd, Ordering::SeqCst);
                use_data_digest.store(dd, Ordering::SeqCst);
                log::info!(
                    "Session from {} entered FullFeaturePhase: initiator={} session_type={:?} HeaderDigest={} DataDigest={} MaxConnections={} MaxRecv={} MaxXmit={} MaxBurst={} FirstBurst={} ImmediateData={} InitialR2T={}",
                    peer,
                    data.params.initiator_name,
                    data.session_type,
                    if hd { "CRC32C" } else { "None" },
                    if dd { "CRC32C" } else { "None" },
                    data.params.max_connections,
                    data.params.max_recv_data_segment_length,
                    data.params.max_xmit_data_segment_length,
                    data.params.max_burst_length,
                    data.params.first_burst_length,
                    data.params.immediate_data,
                    data.params.initial_r2t,
                );
            } else {
                log::info!("Session from {} entered FullFeaturePhase", peer);
            }

            session_entered = true;
            let count = active_sessions.fetch_add(1, Ordering::SeqCst);
            log::debug!("Session count: {} -> {}", count, count + 1);
        }

        // Check if session ended
        if session.is_ended() {
            if let Some(data) = session.data() {
                log::info!(
                    "Session from {} ending (state={}, initiator={})",
                    peer,
                    session.state_name(),
                    data.params.initiator_name
                );
            } else {
                log::info!("Session from {} ending (state={})", peer, session.state_name());
            }
            break;
        }
    }

    // Signal reader thread to stop and clean up (per-connection flag, not global)
    conn_running.store(false, Ordering::SeqCst);
    let _ = write_stream.shutdown(Shutdown::Both);
    let _ = reader_handle.join();
    Ok(session_entered)
}

/// Compute an iSCSI header/data digest the way tgt and Linux open-iscsi put it
/// on the wire: standard CRC32C, emitted in little-endian byte order.
///
/// Interop note (verified against fujita/tgt usr/iscsi/iscsid.c): tgt writes
/// the raw `uint32_t` digest with no `htonl()`, i.e. in *native* byte order.
/// On the little-endian hosts where iSCSI is actually deployed that is
/// little-endian on the wire, even though every other iSCSI field is big-
/// endian -- the well-known digest byte-order divergence. We emit little-endian
/// explicitly, which matches tgt/open-iscsi on x86 and is also correct on a
/// big-endian host (where tgt's native order would be wrong). The CRC32C value
/// uses the standard init/final inversion, already applied by the crc32c crate.
fn iscsi_digest(bytes: &[u8]) -> [u8; 4] {
    crc32c::crc32c(bytes).to_le_bytes()
}

/// Read a PDU from the TCP stream with optional digest verification
#[allow(dead_code)]
fn read_pdu(stream: &mut TcpStream) -> ScsiResult<IscsiPdu> {
    read_pdu_digest(stream, false, false)
}

/// Read a PDU, loading digest flags from atomics AFTER the blocking BHS read.
/// This ensures the reader thread picks up flag changes made by the main thread
/// while the reader was blocked waiting for data (e.g. first PDU after login).
fn read_pdu_atomic(
    stream: &mut TcpStream,
    header_digest: &AtomicBool,
    data_digest: &AtomicBool,
) -> ScsiResult<IscsiPdu> {
    let mut bhs = [0u8; BHS_SIZE];
    stream.read_exact(&mut bhs).map_err(IscsiError::Io)?;
    // Load flags AFTER the blocking read
    let hd = header_digest.load(Ordering::SeqCst);
    let dd = data_digest.load(Ordering::SeqCst);
    read_pdu_after_bhs(stream, bhs, hd, dd)
}

fn read_pdu_digest(stream: &mut TcpStream, header_digest: bool, data_digest: bool) -> ScsiResult<IscsiPdu> {
    let mut bhs = [0u8; BHS_SIZE];
    stream.read_exact(&mut bhs).map_err(IscsiError::Io)?;
    read_pdu_after_bhs(stream, bhs, header_digest, data_digest)
}

fn read_pdu_after_bhs(stream: &mut TcpStream, bhs: [u8; BHS_SIZE], header_digest: bool, data_digest: bool) -> ScsiResult<IscsiPdu> {
    let ahs_length = bhs[4] as usize * 4;
    let data_length = ((bhs[5] as u32) << 16) | ((bhs[6] as u32) << 8) | (bhs[7] as u32);
    let padded_data_len = (data_length as usize).div_ceil(4) * 4;

    // Read the AHS, which sits between the BHS and the header digest on the
    // wire (BHS | AHS | HeaderDigest | Data | DataDigest).
    let mut header = vec![0u8; BHS_SIZE + ahs_length];
    header[..BHS_SIZE].copy_from_slice(&bhs);
    if ahs_length > 0 {
        stream.read_exact(&mut header[BHS_SIZE..]).map_err(IscsiError::Io)?;
    }

    // Header digest covers the BHS *and* AHS (RFC 3720 10.2.1; matches tgt).
    if header_digest {
        let mut hd = [0u8; 4];
        stream.read_exact(&mut hd).map_err(IscsiError::Io)?;
        let expected = iscsi_digest(&header);
        if hd != expected {
            log::error!("Header digest mismatch: expected {:02x?}, got {:02x?}", expected, hd);
            return Err(IscsiError::Protocol("Header digest mismatch".to_string()));
        }
    }

    // Read the padded data segment.
    let mut data = vec![0u8; padded_data_len];
    if padded_data_len > 0 {
        stream.read_exact(&mut data).map_err(IscsiError::Io)?;
    }

    // Data digest covers the padded data only.
    if data_digest && padded_data_len > 0 {
        let mut dd = [0u8; 4];
        stream.read_exact(&mut dd).map_err(IscsiError::Io)?;
        let expected = iscsi_digest(&data);
        if dd != expected {
            log::error!("Data digest mismatch: expected {:02x?}, got {:02x?}", expected, dd);
            return Err(IscsiError::Protocol("Data digest mismatch".to_string()));
        }
    }

    let mut full_pdu = header;
    full_pdu.extend_from_slice(&data);

    let pdu = IscsiPdu::from_bytes(&full_pdu)?;

    if full_pdu.len() >= 48 {
        log::debug!("Received PDU header hex: {}", full_pdu[0..48].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" "));
    }

    Ok(pdu)
}

/// Write a PDU to the TCP stream with optional digests
fn write_pdu(stream: &mut TcpStream, pdu: &IscsiPdu) -> ScsiResult<()> {
    write_pdu_digest(stream, pdu, false, false)
}

fn write_pdu_digest(stream: &mut TcpStream, pdu: &IscsiPdu, header_digest: bool, data_digest: bool) -> ScsiResult<()> {
    let bytes = pdu.to_bytes();

    if bytes.len() >= 48 {
        log::debug!("Sending PDU header hex: {}", bytes[0..48].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" "));
    }

    // BHS | AHS | HeaderDigest | Data | DataDigest.
    let header_end = BHS_SIZE + (bytes[4] as usize) * 4;

    // Write BHS + AHS.
    stream.write_all(&bytes[..header_end]).map_err(IscsiError::Io)?;

    // Header digest covers the BHS + AHS.
    if header_digest {
        stream.write_all(&iscsi_digest(&bytes[..header_end])).map_err(IscsiError::Io)?;
    }

    // Write the padded data segment.
    if bytes.len() > header_end {
        stream.write_all(&bytes[header_end..]).map_err(IscsiError::Io)?;
    }

    // Data digest covers the padded data only.
    if data_digest && bytes.len() > header_end {
        stream.write_all(&iscsi_digest(&bytes[header_end..])).map_err(IscsiError::Io)?;
    }

    stream.flush().map_err(IscsiError::Io)?;
    Ok(())
}

/// Handle PDUs during login phase (using typestate session)
fn handle_login_phase(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    target_name: &str,
    target_address: &str,
    shutting_down: &Arc<AtomicBool>,
    max_sessions: u32,
    active_sessions: &Arc<std::sync::atomic::AtomicUsize>,
) -> ScsiResult<Vec<IscsiPdu>> {
    match pdu.opcode {
        opcode::LOGIN_REQUEST => {
            // Check shutdown and session limits for new logins
            if let Some(data) = session.data() {
                if shutting_down.load(Ordering::SeqCst) && data.isid == [0u8; 6] {
                    log::warn!("Login rejected: target is shutting down");
                    let response = data.create_shutdown_reject(pdu.itt);
                    return Ok(vec![response]);
                }

                if data.isid == [0u8; 6] {
                    let current_sessions = active_sessions.load(Ordering::SeqCst);
                    if current_sessions >= max_sessions as usize {
                        log::warn!("Login rejected: session limit reached ({}/{})", current_sessions, max_sessions);
                        let response = data.create_out_of_resources_reject(pdu.itt);
                        return Ok(vec![response]);
                    }
                }
            }

            // Process login using typestate session
            // We need to take ownership and replace session
            let old_session = std::mem::replace(session, AnySession::new());
            let (new_session, responses) = old_session.process_login(pdu, target_name)?;
            *session = new_session;

            Ok(responses)
        }
        opcode::TEXT_REQUEST => {
            handle_text_request(session, pdu, target_name, target_address)
        }
        _ => {
            log::warn!("Invalid opcode 0x{:02x} during login phase", pdu.opcode);
            if let Some(data) = session.data() {
                let response = data.create_invalid_request_during_login_reject(pdu.itt);
                Ok(vec![response])
            } else {
                Ok(vec![])
            }
        }
    }
}

/// Handle PDUs during full feature phase
fn handle_full_feature_phase<D: ScsiBlockDevice>(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    device: &Arc<Mutex<D>>,
    target_name: &str,
    target_address: &str,
) -> ScsiResult<Vec<IscsiPdu>> {
    match pdu.opcode {
        opcode::SCSI_COMMAND => {
            handle_scsi_command(session, pdu, device, target_name)
        }
        opcode::SCSI_DATA_OUT => {
            handle_scsi_data_out(session, pdu, device)
        }
        opcode::NOP_OUT => {
            let response = session.process_nop_out(pdu)?;
            Ok(vec![response])
        }
        opcode::LOGOUT_REQUEST => {
            // Process logout - this transitions session state
            let old_session = std::mem::replace(session, AnySession::new());
            let (new_session, response) = old_session.process_logout(pdu)?;
            *session = new_session;
            Ok(vec![response])
        }
        opcode::TEXT_REQUEST => {
            handle_text_request(session, pdu, target_name, target_address)
        }
        opcode::TASK_MANAGEMENT_REQUEST => {
            handle_task_management(session, pdu)
        }
        _ => {
            log::warn!("Unsupported opcode 0x{:02x} in full feature phase", pdu.opcode);
            Ok(vec![])
        }
    }
}

fn is_bulk_io_opcode(opcode: u8) -> bool {
    matches!(opcode, 0x08 | 0x0a | 0x28 | 0x2a | 0x88 | 0x8a) // READ/WRITE 6/10/16
}

fn scsi_opcode_name(opcode: u8) -> &'static str {
    match opcode {
        0x00 => "TEST_UNIT_READY",
        0x03 => "REQUEST_SENSE",
        0x08 => "READ_6",
        0x0a => "WRITE_6",
        0x12 => "INQUIRY",
        0x15 => "MODE_SELECT_6",
        0x1a => "MODE_SENSE_6",
        0x1b => "START_STOP_UNIT",
        0x25 => "READ_CAPACITY_10",
        0x28 => "READ_10",
        0x2a => "WRITE_10",
        0x2f => "VERIFY_10",
        0x35 => "SYNC_CACHE_10",
        0x3b => "WRITE_BUFFER",
        0x3c => "READ_BUFFER",
        0x55 => "MODE_SELECT_10",
        0x5a => "MODE_SENSE_10",
        0x88 => "READ_16",
        0x8a => "WRITE_16",
        0x91 => "SYNC_CACHE_16",
        0x9e => "SERVICE_ACTION_IN_16",
        0xa0 => "REPORT_LUNS",
        _ => "UNKNOWN",
    }
}

fn log_scsi_command(cmd: &ScsiCommandPdu, data_len: usize) {
    let name = scsi_opcode_name(cmd.cdb[0]);
    if is_bulk_io_opcode(cmd.cdb[0]) {
        log::debug!(
            "SCSI {}: CDB[0]=0x{:02x}, LUN=0x{:016x}, ITT=0x{:08x}, ExpLen={}, read={}, write={}, final={}, data_len={}",
            name, cmd.cdb[0], cmd.lun, cmd.itt, cmd.expected_data_length, cmd.read, cmd.write, cmd.final_flag, data_len
        );
    } else {
        let cdb_preview = &cmd.cdb[..cmd.cdb.len().min(16)];
        log::info!(
            "SCSI {}: CDB={:02x?}, LUN=0x{:016x}, ITT=0x{:08x}, ExpLen={}, data_len={}",
            name, cdb_preview, cmd.lun, cmd.itt, cmd.expected_data_length, data_len
        );
    }
}

/// Handle SCSI Command PDU
fn handle_scsi_command<D: ScsiBlockDevice>(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    device: &Arc<Mutex<D>>,
    target_name: &str,
) -> ScsiResult<Vec<IscsiPdu>> {
    let cmd = pdu.parse_scsi_command()?;
    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("Session not in FullFeaturePhase".to_string()))?;

    log_scsi_command(&cmd, pdu.data.len());

    // Validate LUN
    if cmd.lun != 0 {
        log::warn!("Command 0x{:02x} ({}) to invalid LUN: 0x{:016x}", cmd.cdb[0], scsi_opcode_name(cmd.cdb[0]), cmd.lun);
        let sense = crate::scsi::SenseData::new(
            crate::scsi::sense_key::ILLEGAL_REQUEST,
            crate::scsi::asc::LOGICAL_UNIT_NOT_SUPPORTED,
            0,
        );
        return Ok(vec![IscsiPdu::scsi_response(
            cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
        )]);
    }

    // Validate CmdSN
    let cmd_sn = BigEndian::read_u32(&pdu.specific[4..8]);
    if !data.validate_cmd_sn(cmd_sn) {
        log::warn!("Invalid CmdSN: {}, expected: {}", cmd_sn, data.exp_cmd_sn);
    }

    let opcode = cmd.cdb[0];
    let is_sync_cache = opcode == 0x35 || opcode == 0x91;
    let is_write_cmd = matches!(opcode, 0x0a | 0x2a | 0x8a);

    // Handle WRITE commands
    if is_write_cmd {
        return handle_write_command(data, pdu, &cmd, device);
    }

    // Handle non-write commands
    let response = if opcode == 0x03 {
        // REQUEST SENSE
        log::info!("REQUEST SENSE called");
        if cmd.cdb.len() < 6 {
            ScsiResponse::check_condition(crate::scsi::SenseData::invalid_command())
        } else {
            let alloc_len = cmd.cdb[4] as usize;
            let mut sense_data = match &data.last_sense_data {
                Some(bytes) => bytes.clone(),
                None => crate::scsi::SenseData::new(
                    crate::scsi::sense_key::NO_SENSE,
                    crate::scsi::asc::NO_ADDITIONAL_SENSE,
                    0,
                ).to_bytes(),
            };
            sense_data.truncate(alloc_len.min(sense_data.len()));
            ScsiResponse::good(sense_data)
        }
    } else if is_sync_cache {
        let mut device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        device_guard.flush()?;
        ScsiResponse::good_no_data()
    } else {
        let device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        ScsiHandler::handle_command_with_target(&cmd.cdb, &*device_guard, None, Some(target_name))?
    };

    // Build response PDU(s)
    build_scsi_response(data, &cmd, response)
}

fn handle_write_command<D: ScsiBlockDevice>(
    data: &mut SessionData,
    pdu: &IscsiPdu,
    cmd: &ScsiCommandPdu,
    device: &Arc<Mutex<D>>,
) -> ScsiResult<Vec<IscsiPdu>> {
    let opcode = cmd.cdb[0];

    let (lba, transfer_length) = match opcode {
        0x0a | 0x2a => {
            if opcode == 0x0a && cmd.cdb.len() >= 6 {
                let lba_21 = ((cmd.cdb[1] as u32 & 0x1F) << 16)
                           | ((cmd.cdb[2] as u32) << 8)
                           | (cmd.cdb[3] as u32);
                (lba_21 as u64, cmd.cdb[4] as u32)
            } else if opcode == 0x2a && cmd.cdb.len() >= 10 {
                let lba = BigEndian::read_u32(&cmd.cdb[2..6]) as u64;
                let length = BigEndian::read_u16(&cmd.cdb[7..9]) as u32;
                (lba, length)
            } else {
                (0, 0)
            }
        }
        0x8a => {
            if cmd.cdb.len() >= 16 {
                let lba = BigEndian::read_u64(&cmd.cdb[2..10]);
                let length = BigEndian::read_u32(&cmd.cdb[10..14]);
                (lba, length)
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };

    if transfer_length > 0 {
        let device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        let block_size = device_guard.block_size();
        drop(device_guard);

        let expected_data_len = transfer_length as usize * block_size as usize;
        let bytes_received = pdu.data.len() as u32;

        // Check if this is a single-PDU write (all data fits in immediate data)
        if bytes_received as usize == expected_data_len {
            // Single-PDU write - write directly
            let mut device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
            if let Err(e) = device_guard.write(lba, &pdu.data, block_size) {
                log::error!("Write failed: {}", e);
                let sense = crate::scsi::SenseData::medium_error();
                return Ok(vec![IscsiPdu::scsi_response(
                    cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
                    pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
                )]);
            }
            return Ok(vec![IscsiPdu::scsi_response(
                cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
                pdu::scsi_status::GOOD, 0, 0, None,
            )]);
        }

        // Multi-PDU write - create buffer and copy immediate data
        let mut buffer = vec![0u8; expected_data_len];
        if !pdu.data.is_empty() {
            buffer[..pdu.data.len()].copy_from_slice(&pdu.data);
        }

        let ttt = data.next_target_transfer_tag();

        data.pending_writes.insert(cmd.itt, PendingWrite {
            lba, transfer_length, block_size, bytes_received, ttt, r2t_sn: 0, lun: cmd.lun,
            buffer,
            next_r2t_offset: bytes_received,
            expected_data_len: expected_data_len as u32,
            completed: false,
            r2t_pending: false,
        });

        // If F bit is clear, the initiator will send more unsolicited Data-Out
        // PDUs. Don't send R2T yet — the Data-Out handler will send R2T for
        // any remaining data after the unsolicited burst completes (F=true).
        if !cmd.final_flag {
            return Ok(vec![]);
        }

        // F bit set — no more unsolicited data. Send R2T for remainder.
        let max_burst = data.params.max_burst_length;
        let remaining = expected_data_len as u32 - bytes_received;
        let request_len = remaining.min(max_burst);

        if let Some(pending) = data.pending_writes.get_mut(&cmd.itt) {
            pending.next_r2t_offset = bytes_received + request_len;
            pending.r2t_sn = 1;
            pending.r2t_pending = true;
        }

        let r2t = IscsiPdu::r2t(
            cmd.lun, cmd.itt, ttt, data.stat_sn,
            data.exp_cmd_sn, data.max_cmd_sn,
            0, bytes_received, request_len,
        );

        return Ok(vec![r2t]);
    }

    Ok(vec![IscsiPdu::scsi_response(
        cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
        pdu::scsi_status::GOOD, 0, 0, None,
    )])
}

fn build_scsi_response(
    data: &mut SessionData,
    cmd: &ScsiCommandPdu,
    response: ScsiResponse,
) -> ScsiResult<Vec<IscsiPdu>> {
    let mut responses = Vec::new();

    if !response.data.is_empty() {
        let max_data_seg = data.params.max_xmit_data_segment_length as usize;
        let mut offset = 0u32;
        let mut data_sn = 0u32;

        while offset < response.data.len() as u32 {
            let remaining = response.data.len() - offset as usize;
            let chunk_size = remaining.min(max_data_seg);
            let is_final = offset as usize + chunk_size >= response.data.len();

            let chunk = response.data[offset as usize..offset as usize + chunk_size].to_vec();
            let pdu_stat_sn = if is_final { data.next_stat_sn() } else { 0 };

            let data_in = IscsiPdu::scsi_data_in(
                cmd.itt, 0xFFFF_FFFF, pdu_stat_sn,
                data.exp_cmd_sn, data.max_cmd_sn,
                data_sn, offset, chunk, is_final,
                if is_final { Some(response.status) } else { None },
            );

            responses.push(data_in);
            offset += chunk_size as u32;
            data_sn += 1;
        }
    } else {
        let sense_data = response.sense.as_ref().map(|s| s.to_bytes());

        if response.status == pdu::scsi_status::CHECK_CONDITION {
            if let Some(ref sd) = response.sense {
                let sense_bytes = sd.to_bytes();
                log::info!("CHECK CONDITION: sense_key=0x{:02x}, asc=0x{:02x}", sd.sense_key, sd.asc);
                data.last_sense_data = Some(sense_bytes);
            }
        } else {
            data.last_sense_data = None;
        }

        let scsi_resp = IscsiPdu::scsi_response(
            cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            response.status, 0, 0, sense_data.as_deref(),
        );
        responses.push(scsi_resp);
    }

    Ok(responses)
}

/// Handle SCSI Data-Out PDU
fn handle_scsi_data_out<D: ScsiBlockDevice>(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    device: &Arc<Mutex<D>>,
) -> ScsiResult<Vec<IscsiPdu>> {
    let data_out = pdu.parse_scsi_data_out()?;
    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("Session not in FullFeaturePhase".to_string()))?;

    let pending = data.pending_writes.get_mut(&data_out.itt);
    if pending.is_none() {
        log::debug!("Data-Out for unknown ITT=0x{:08x}, ignoring", data_out.itt);
        return Ok(vec![]);
    }
    let pending = pending.unwrap();
    if pending.completed {
        log::debug!("Data-Out for completed ITT=0x{:08x}, absorbing", data_out.itt);
        if data_out.final_flag {
            data.pending_writes.remove(&data_out.itt);
        }
        return Ok(vec![]);
    }

    let block_size = pending.block_size;
    let transfer_length = pending.transfer_length;
    let lba = pending.lba;
    let total_expected = transfer_length * block_size;

    // Copy data into buffer at the correct offset
    let start_offset = data_out.buffer_offset as usize;
    let end_offset = start_offset + data_out.data.len();

    if end_offset > pending.buffer.len() {
        log::error!("DATA-OUT offset {} + len {} exceeds buffer size {}",
            data_out.buffer_offset, data_out.data.len(), pending.buffer.len());
        let sense = crate::scsi::SenseData::medium_error();
        return Ok(vec![IscsiPdu::scsi_response(
            data_out.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
        )]);
    }

    pending.buffer[start_offset..end_offset].copy_from_slice(&data_out.data);
    if end_offset as u32 > pending.bytes_received {
        pending.bytes_received = end_offset as u32;
    }

    log::debug!("Data-Out: ITT=0x{:08x} off={} len={} F={} recv={}/{} TTT=0x{:08x}",
        data_out.itt, start_offset, data_out.data.len(), data_out.final_flag,
        pending.bytes_received, total_expected, data_out.ttt);

    // Check if transfer is complete — require F bit to avoid completing
    // while unsolicited Data-Out PDUs are still in flight
    if data_out.final_flag && pending.bytes_received >= total_expected {
        let itt = data_out.itt;
        log::debug!("Write complete: ITT=0x{:08x} bytes={}/{}", itt, pending.bytes_received, total_expected);
        let buffer = pending.buffer.clone();
        pending.completed = true;

        // Now write the complete buffer to the device in one operation
        let mut device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        let write_result = device_guard.write(lba, &buffer, block_size);
        drop(device_guard);

        let (status, sense) = match write_result {
            Ok(()) => (scsi_status::GOOD, None),
            Err(e) => {
                log::error!("Write failed: {}", e);
                (pdu::scsi_status::CHECK_CONDITION, Some(crate::scsi::SenseData::medium_error().to_bytes()))
            }
        };

        return Ok(vec![IscsiPdu::scsi_response(
            itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            status, 0, 0, sense.as_deref(),
        )]);
    }

    // F bit set — this burst is done. Mark R2T as no longer pending.
    if data_out.final_flag {
        pending.r2t_pending = false;
    }

    // Need more data and no R2T outstanding — send one.
    // Only after F=1 (current burst complete); don't interrupt unsolicited bursts.
    if data_out.final_flag && !pending.r2t_pending && pending.bytes_received < total_expected {
        let max_burst = data.params.max_burst_length;
        let remaining = total_expected - pending.bytes_received;
        let request_len = remaining.min(max_burst);
        let current_offset = pending.bytes_received;
        let r2t_sn = pending.r2t_sn;
        let ttt = pending.ttt;
        let lun = pending.lun;
        let itt = data_out.itt;

        // Update pending write state
        pending.next_r2t_offset += request_len;
        pending.r2t_sn += 1;

        // Send next R2T
        pending.r2t_pending = true;

        let r2t = IscsiPdu::r2t(
            lun, itt, ttt, data.stat_sn,
            data.exp_cmd_sn, data.max_cmd_sn,
            r2t_sn, current_offset, request_len,
        );

        return Ok(vec![r2t]);
    }

    // Waiting for more DATA-OUT PDUs
    Ok(vec![])
}

/// Handle Text Request
fn handle_text_request(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    target_name: &str,
    target_address: &str,
) -> ScsiResult<Vec<IscsiPdu>> {
    handle_text_request_multi(session, pdu, target_name, &[target_address.to_string()])
}

/// SendTargets may list multiple TargetAddress values (MPIO portals).
fn handle_text_request_multi(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    target_name: &str,
    portals: &[String],
) -> ScsiResult<Vec<IscsiPdu>> {
    let text_req = pdu.parse_text_request()?;

    let is_send_targets = text_req.parameters.iter()
        .any(|(k, v)| k == "SendTargets" && (v == "All" || v.is_empty()));

    let response_params = if is_send_targets {
        let mut params = vec![("TargetName".to_string(), target_name.to_string())];
        for portal in portals {
            params.push(("TargetAddress".to_string(), format!("{},1", portal)));
        }
        params
    } else {
        vec![]
    };

    let response_data = serialize_text_parameters(&response_params);

    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("No session data".to_string()))?;

    let response = IscsiPdu::text_response(
        text_req.itt, 0xFFFF_FFFF,
        data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
        true, response_data,
    );

    Ok(vec![response])
}

/// Handle Task Management Request
fn handle_task_management(
    session: &mut AnySession,
    pdu: &IscsiPdu,
) -> ScsiResult<Vec<IscsiPdu>> {
    let function = pdu.flags & 0x7F;
    log::debug!("Task Management: function={}", function);

    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("No session data".to_string()))?;

    let mut response = IscsiPdu::new();
    response.opcode = opcode::TASK_MANAGEMENT_RESPONSE;
    response.flags = flags::FINAL;
    response.itt = pdu.itt;

    response.specific[0] = 0x00; // function complete
    response.specific[4..8].copy_from_slice(&data.next_stat_sn().to_be_bytes());
    response.specific[8..12].copy_from_slice(&data.exp_cmd_sn.to_be_bytes());
    response.specific[12..16].copy_from_slice(&data.max_cmd_sn.to_be_bytes());

    Ok(vec![response])
}

/// Builder for configuring an iSCSI target
pub struct IscsiTargetBuilder<D: ScsiBlockDevice> {
    bind_addr: Option<String>,
    advertise_addr: Option<String>,
    target_name: Option<String>,
    target_alias: Option<String>,
    auth_config: crate::auth::AuthConfig,
    max_connections: Option<u32>,
    max_sessions: Option<u32>,
    allowed_initiators: Option<Vec<String>>,
    _phantom: std::marker::PhantomData<D>,
}

impl<D: ScsiBlockDevice> IscsiTargetBuilder<D> {
    fn new() -> Self {
        Self {
            bind_addr: None,
            advertise_addr: None,
            target_name: None,
            target_alias: None,
            auth_config: crate::auth::AuthConfig::None,
            max_connections: None,
            max_sessions: None,
            allowed_initiators: None,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn bind_addr(mut self, addr: &str) -> Self {
        self.bind_addr = Some(addr.to_string());
        self
    }

    /// Host:port returned in SendTargets `TargetAddress` (and related login
    /// text). Use this when the listen address is not reachable by clients
    /// (e.g. Docker port publish, NAT). Defaults to the connection's local_addr.
    pub fn advertise_addr(mut self, addr: &str) -> Self {
        self.advertise_addr = Some(addr.to_string());
        self
    }

    pub fn target_name(mut self, name: &str) -> Self {
        self.target_name = Some(name.to_string());
        self
    }

    pub fn target_alias(mut self, alias: &str) -> Self {
        self.target_alias = Some(alias.to_string());
        self
    }

    pub fn with_auth(mut self, auth_config: crate::auth::AuthConfig) -> Self {
        self.auth_config = auth_config;
        self
    }

    pub fn max_connections(mut self, max: u32) -> Self {
        self.max_connections = Some(max);
        self
    }

    pub fn max_sessions(mut self, max: u32) -> Self {
        self.max_sessions = Some(max);
        self
    }

    pub fn allowed_initiators(mut self, initiators: Vec<String>) -> Self {
        self.allowed_initiators = Some(initiators);
        self
    }

    pub fn build(self, device: D) -> ScsiResult<IscsiTarget<D>> {
        let bind_addr = self.bind_addr.unwrap_or_else(|| format!("0.0.0.0:{}", ISCSI_PORT));
        let target_name = self.target_name.unwrap_or_else(|| "iqn.2025-12.local:storage.default".to_string());
        let target_alias = self.target_alias.unwrap_or_else(|| "iSCSI Target".to_string());

        if !target_name.starts_with("iqn.") && !target_name.starts_with("eui.") && !target_name.starts_with("naa.") {
            return Err(IscsiError::Config(
                "target_name must be in IQN, EUI, or NAA format".to_string()
            ));
        }

        Ok(IscsiTarget {
            bind_addr,
            advertise_addr: self.advertise_addr,
            target_name,
            target_alias,
            device: Arc::new(Mutex::new(device)),
            running: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            auth_config: self.auth_config,
            max_connections: self.max_connections.unwrap_or(16),
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_sessions: self.max_sessions.unwrap_or(256),
            active_sessions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            allowed_initiators: self.allowed_initiators,
        })
    }
}

// ============================================================================
// Multi-Target iSCSI Server
// ============================================================================

/// Target configuration for multi-target server
struct TargetInfo {
    device: Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
    alias: String,
    auth_config: crate::auth::AuthConfig,
    allowed_initiators: Option<Vec<String>>,
}

/// Callbacks for FullFeature session lifecycle (used for metrics / admin).
pub struct SessionEvent {
    pub target_iqn: String,
    pub initiator_iqn: String,
    /// TCP peer (`ip:port`) for this connection.
    pub peer: String,
}

pub trait SessionEventSink: Send + Sync {
    fn on_session_start(&self, event: &SessionEvent);
    fn on_session_end(&self, event: &SessionEvent);
}

/// Multi-target iSCSI server
///
/// Serves multiple targets on a single port with IQN-based routing
pub struct IscsiServer {
    bind_addr: String,
    /// Client-reachable portal addresses for SendTargets (host:port).
    /// Empty = use the socket local_addr alone.
    portal_addrs: Vec<String>,
    targets: Arc<Mutex<std::collections::HashMap<String, TargetInfo>>>,
    running: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    max_connections: u32,
    active_connections: Arc<std::sync::atomic::AtomicUsize>,
    max_sessions: u32,
    active_sessions: Arc<std::sync::atomic::AtomicUsize>,
    session_events: Option<Arc<dyn SessionEventSink>>,
}

impl IscsiServer {
    /// Create a new builder for configuring the server
    pub fn builder() -> IscsiServerBuilder {
        IscsiServerBuilder::new()
    }

    /// Run the multi-target iSCSI server
    ///
    /// This blocks the current thread and processes incoming connections.
    pub fn run(&self) -> ScsiResult<()> {
        let targets = self.targets.lock().unwrap();
        let target_count = targets.len();
        drop(targets);

        log::info!("Multi-target iSCSI server starting on {}", self.bind_addr);
        log::info!("Serving {} target(s)", target_count);

        let listener = TcpListener::bind(&self.bind_addr)
            .map_err(IscsiError::Io)?;

        listener.set_nonblocking(true)
            .map_err(IscsiError::Io)?;

        self.running.store(true, Ordering::SeqCst);

        log::info!("iSCSI server listening on {}", self.bind_addr);

        while self.running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, addr)) => {
                    log::info!("New connection from {}", addr);

                    // Check connection limit
                    let current = self.active_connections.fetch_add(1, Ordering::SeqCst);
                    if current >= self.max_connections as usize {
                        log::warn!("Connection rejected from {}: too many connections ({}/{})",
                            addr, current + 1, self.max_connections);
                        self.active_connections.fetch_sub(1, Ordering::SeqCst);
                        let _ = send_connection_limit_reject(stream);
                        continue;
                    }

                    log::debug!("Accepted connection from {} ({}/{} active)",
                        addr, current + 1, self.max_connections);

                    let targets = Arc::clone(&self.targets);
                    let running = Arc::clone(&self.running);
                    let shutting_down = Arc::clone(&self.shutting_down);
                    let active_connections = Arc::clone(&self.active_connections);
                    let max_sessions = self.max_sessions;
                    let active_sessions = Arc::clone(&self.active_sessions);
                    let portal_addrs = self.portal_addrs.clone();
                    let session_events = self.session_events.clone();

                    thread::spawn(move || {
                        let outcome = match handle_multi_target_connection(
                            stream,
                            targets,
                            portal_addrs,
                            running,
                            shutting_down,
                            max_sessions,
                            Arc::clone(&active_sessions),
                            session_events.clone(),
                            addr,
                        ) {
                            Ok(outcome) => outcome,
                            Err(e) => {
                                log::error!("Connection from {} failed: {}", addr, e);
                                MultiTargetOutcome {
                                    session_entered: false,
                                    target_iqn: None,
                                    initiator_iqn: None,
                                }
                            }
                        };

                        log::info!(
                            "Connection closed from {} (full_feature={})",
                            addr,
                            outcome.session_entered
                        );

                        active_connections.fetch_sub(1, Ordering::SeqCst);

                        if outcome.session_entered {
                            active_sessions.fetch_sub(1, Ordering::SeqCst);
                            if let (Some(sink), Some(iqn)) =
                                (session_events.as_ref(), outcome.target_iqn.as_deref())
                            {
                                sink.on_session_end(&SessionEvent {
                                    target_iqn: iqn.to_string(),
                                    initiator_iqn: outcome
                                        .initiator_iqn
                                        .unwrap_or_default(),
                                    peer: addr.to_string(),
                                });
                            }
                        }
                    });
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => {
                    log::error!("Accept error: {}", e);
                }
            }
        }

        log::info!("Multi-target iSCSI server shutting down");
        Ok(())
    }

    /// Get the current number of active connections
    pub fn active_connection_count(&self) -> usize {
        self.active_connections.load(Ordering::SeqCst)
    }

    /// Get the current number of active sessions
    pub fn active_session_count(&self) -> usize {
        self.active_sessions.load(Ordering::SeqCst)
    }

    /// Initiate graceful shutdown
    pub fn shutdown_gracefully(&self) {
        log::info!("Initiating graceful shutdown - new logins will be rejected");
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Signal the server to stop immediately
    pub fn stop(&self) {
        log::info!("Stopping multi-target iSCSI server");
        self.running.store(false, Ordering::SeqCst);
    }

    /// Check if the server is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Check if the server is in graceful shutdown mode
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

/// Resolve portal list for SendTargets. Configured portals win; otherwise socket local_addr.
fn resolve_portal_addresses(stream: &TcpStream, configured: &[String]) -> ScsiResult<Vec<String>> {
    if !configured.is_empty() {
        return Ok(configured.to_vec());
    }
    Ok(vec![stream.local_addr().map_err(IscsiError::Io)?.to_string()])
}

struct MultiTargetOutcome {
    session_entered: bool,
    target_iqn: Option<String>,
    initiator_iqn: Option<String>,
}

/// Handle a connection with multi-target routing
fn handle_multi_target_connection(
    mut stream: TcpStream,
    targets: Arc<Mutex<std::collections::HashMap<String, TargetInfo>>>,
    portal_addrs: Vec<String>,
    running: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    max_sessions: u32,
    active_sessions: Arc<std::sync::atomic::AtomicUsize>,
    session_events: Option<Arc<dyn SessionEventSink>>,
    peer: std::net::SocketAddr,
) -> ScsiResult<MultiTargetOutcome> {
    stream.set_nonblocking(false).map_err(IscsiError::Io)?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(IscsiError::Io)?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(IscsiError::Io)?;

    let portals = resolve_portal_addresses(&stream, &portal_addrs)?;
    let primary_portal = portals.first().cloned().unwrap_or_else(|| "0.0.0.0:3260".into());
    log::info!(
        "Handling multi-target connection from {} (local={}, portals={:?})",
        peer,
        stream.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into()),
        portals
    );

    // Read first PDU to extract target name (login has no digests yet).
    let first_pdu = match read_pdu_digest(&mut stream, false, false) {
        Ok(pdu) => pdu,
        Err(e) => {
            log::error!("Failed to read first PDU from {}: {}", peer, e);
            return Ok(MultiTargetOutcome {
                session_entered: false,
                target_iqn: None,
                initiator_iqn: None,
            });
        }
    };

    if first_pdu.opcode != opcode::LOGIN_REQUEST {
        log::warn!("First PDU from {} is not login request: 0x{:02x}", peer, first_pdu.opcode);
        return Ok(MultiTargetOutcome {
            session_entered: false,
            target_iqn: None,
            initiator_iqn: None,
        });
    }

    let login_req = first_pdu.parse_login_request()?;
    let target_name = login_req
        .parameters
        .iter()
        .find(|(k, _)| k == "TargetName")
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty());

    if target_name.is_none() {
        log::info!("Discovery session from {}", peer);
        let _ = handle_discovery_session(stream, targets, &portals, first_pdu, peer)?;
        return Ok(MultiTargetOutcome {
            session_entered: false,
            target_iqn: None,
            initiator_iqn: None,
        });
    }

    let target_name = target_name.unwrap();
    log::info!("Login request from {} for target: {}", peer, target_name);

    let targets_lock = targets.lock().unwrap();
    let target_info = match targets_lock.get(target_name) {
        Some(info) => info,
        None => {
            log::warn!("Target not found: {}", target_name);
            let data = SessionData::default();
            let reject_pdu = data.create_login_reject(
                first_pdu.itt,
                pdu::login_status::INITIATOR_ERROR,
                0x03, // Target not found
            );
            let _ = write_pdu_digest(&mut stream, &reject_pdu, false, false);
            return Ok(MultiTargetOutcome {
                session_entered: false,
                target_iqn: None,
                initiator_iqn: None,
            });
        }
    };

    let device = Arc::clone(&target_info.device);
    let alias = target_info.alias.clone();
    let auth_config = target_info.auth_config.clone();
    let allowed_initiators = target_info.allowed_initiators.clone();
    let target_iqn = target_name.to_string();
    drop(targets_lock);

    log::info!("Routing {} to target: {} ({})", peer, target_iqn, alias);

    let (session_entered, initiator_iqn) = handle_connection_with_first_pdu_boxed(
        stream,
        device,
        &target_iqn,
        &alias,
        auth_config,
        running,
        shutting_down,
        max_sessions,
        active_sessions,
        allowed_initiators,
        first_pdu,
        &primary_portal,
        &portals,
        session_events,
        peer,
    )?;

    Ok(MultiTargetOutcome {
        session_entered,
        target_iqn: Some(target_iqn),
        initiator_iqn,
    })
}

/// Handle discovery session (SessionType=Discovery) on the shared portal.
fn handle_discovery_session(
    mut stream: TcpStream,
    targets: Arc<Mutex<std::collections::HashMap<String, TargetInfo>>>,
    portals: &[String],
    first_pdu: IscsiPdu,
    peer: std::net::SocketAddr,
) -> ScsiResult<bool> {
    let mut session = AnySession::new();
    let mut use_hd = false;
    let mut use_dd = false;

    let mut pending = Some(first_pdu);
    loop {
        let pdu = if let Some(p) = pending.take() {
            p
        } else {
            match read_pdu_digest(&mut stream, use_hd, use_dd) {
                Ok(pdu) => pdu,
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::info!("Discovery peer {} closed connection (EOF)", peer);
                    break;
                }
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    continue;
                }
                Err(e) => {
                    log::debug!("Discovery session from {} ended: {}", peer, e);
                    break;
                }
            }
        };

        log::info!(
            "RX {} from {} (discovery, opcode 0x{:02x}, state={})",
            pdu.opcode_name(),
            peer,
            pdu.opcode,
            session.state_name()
        );

        let was_ff = session.is_full_feature();
        let responses = if session.is_login_phase() || matches!(pdu.opcode, opcode::LOGIN_REQUEST) {
            if pdu.opcode != opcode::LOGIN_REQUEST {
                log::warn!("Unexpected opcode 0x{:02x} during discovery login from {}", pdu.opcode, peer);
                break;
            }
            let old = std::mem::replace(&mut session, AnySession::new());
            let (new_session, responses) = old.process_login(&pdu, "")?;
            session = new_session;
            responses
        } else if session.is_full_feature() {
            match pdu.opcode {
                opcode::TEXT_REQUEST => {
                    let text_req = pdu.parse_text_request()?;
                    let is_send_targets = text_req.parameters.iter().any(|(k, v)| {
                        k == "SendTargets" && (v == "All" || v.is_empty() || v == "all")
                    });

                    let data = session
                        .data_mut()
                        .ok_or_else(|| IscsiError::Protocol("No session data".to_string()))?;
                    let response_data = if is_send_targets {
                        let targets_lock = targets.lock().unwrap();
                        let mut response_params = Vec::new();
                        for (iqn, _) in targets_lock.iter() {
                            response_params.push(("TargetName".to_string(), iqn.clone()));
                            // All portals for this IQN (MPIO / multi-instance discovery).
                            for portal in portals {
                                response_params
                                    .push(("TargetAddress".to_string(), format!("{},1", portal)));
                            }
                        }
                        drop(targets_lock);
                        log::info!(
                            "SendTargets for {}: {} target(s), {} portal(s) {:?}",
                            peer,
                            response_params.iter().filter(|(k, _)| k == "TargetName").count(),
                            portals.len(),
                            portals
                        );
                        serialize_text_parameters(&response_params)
                    } else {
                        vec![]
                    };
                    vec![IscsiPdu::text_response(
                        pdu.itt,
                        0xFFFF_FFFF,
                        data.next_stat_sn(),
                        data.exp_cmd_sn,
                        data.max_cmd_sn,
                        true,
                        response_data,
                    )]
                }
                opcode::NOP_OUT => {
                    vec![session.process_nop_out(&pdu)?]
                }
                opcode::LOGOUT_REQUEST => {
                    let old = std::mem::replace(&mut session, AnySession::new());
                    let (_ended, response) = old.process_logout(&pdu)?;
                    let responses = vec![response];
                    for resp in &responses {
                        write_pdu_digest(&mut stream, resp, use_hd, use_dd)?;
                    }
                    break;
                }
                _ => {
                    log::warn!("Unexpected opcode during discovery FFP: 0x{:02x}", pdu.opcode);
                    break;
                }
            }
        } else {
            log::info!("Discovery session from {} ended (state={})", peer, session.state_name());
            break;
        };

        for resp in &responses {
            log::info!(
                "TX {} to {} (discovery, opcode 0x{:02x})",
                resp.opcode_name(),
                peer,
                resp.opcode
            );
            write_pdu_digest(&mut stream, resp, use_hd, use_dd)?;
        }

        if !was_ff && session.is_full_feature() {
            if let Some(data) = session.data() {
                use_hd = matches!(data.params.header_digest, crate::session::DigestType::CRC32C);
                use_dd = matches!(data.params.data_digest, crate::session::DigestType::CRC32C);
                log::info!(
                    "Discovery session from {} entered FullFeaturePhase (HeaderDigest={}, DataDigest={})",
                    peer,
                    if use_hd { "CRC32C" } else { "None" },
                    if use_dd { "CRC32C" } else { "None" }
                );
            }
        }

        if session.is_ended() {
            break;
        }
    }

    Ok(false)
}

/// Normal (non-discovery) session after TargetName routing, with digest support.
///
/// Uses the same reader-thread + inline NOP pattern as `handle_connection` so
/// keepalives are answered while SCSI I/O (e.g. S3) blocks the main loop.
fn handle_connection_with_first_pdu_boxed(
    mut stream: TcpStream,
    device: Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
    target_name: &str,
    target_alias: &str,
    auth_config: crate::auth::AuthConfig,
    running: Arc<AtomicBool>,
    shutting_down: Arc<AtomicBool>,
    max_sessions: u32,
    active_sessions: Arc<std::sync::atomic::AtomicUsize>,
    allowed_initiators: Option<Vec<String>>,
    first_pdu: IscsiPdu,
    portal: &str,
    portals: &[String],
    session_events: Option<Arc<dyn SessionEventSink>>,
    peer: std::net::SocketAddr,
) -> ScsiResult<(bool, Option<String>)> {
    let mut session =
        AnySession::new_configured(auth_config, target_name, target_alias, allowed_initiators);
    let mut session_entered = false;
    let mut initiator_iqn: Option<String> = None;

    let mut write_stream = stream.try_clone().map_err(IscsiError::Io)?;
    let nop_write_stream = Arc::new(Mutex::new(stream.try_clone().map_err(IscsiError::Io)?));

    let use_header_digest = Arc::new(AtomicBool::new(false));
    let use_data_digest = Arc::new(AtomicBool::new(false));
    let reader_hd = use_header_digest.clone();
    let reader_dd = use_data_digest.clone();
    let nop_hd = use_header_digest.clone();
    let nop_dd = use_data_digest.clone();

    let (pdu_tx, pdu_rx) = std::sync::mpsc::channel::<IscsiPdu>();
    let conn_running = Arc::new(AtomicBool::new(true));
    let reader_running = conn_running.clone();
    let nop_writer = nop_write_stream.clone();

    // First PDU was already consumed for TargetName routing; reader handles the rest.
    let reader_handle = thread::spawn(move || {
        while reader_running.load(Ordering::SeqCst) {
            let pdu = match read_pdu_atomic(&mut stream, &reader_hd, &reader_dd) {
                Ok(pdu) => pdu,
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    log::info!("Peer {} closed connection (EOF)", peer);
                    break;
                }
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    continue;
                }
                Err(IscsiError::Io(ref e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    continue;
                }
                Err(e) => {
                    log::error!("Error reading PDU from {}: {}", peer, e);
                    break;
                }
            };

            if pdu.opcode == opcode::NOP_OUT {
                log::debug!("NOP-Out received on shared portal, responding inline");
                let mut resp = IscsiPdu::new();
                resp.opcode = 0x20; // NOP-In
                resp.flags = 0x80; // Final
                resp.itt = pdu.itt;
                resp.specific[0..4].copy_from_slice(&pdu.specific[0..4]); // TTT
                if let Ok(mut writer) = nop_writer.lock() {
                    let _ = write_pdu_digest(
                        &mut *writer,
                        &resp,
                        nop_hd.load(Ordering::SeqCst),
                        nop_dd.load(Ordering::SeqCst),
                    );
                }
                continue;
            }

            if pdu_tx.send(pdu).is_err() {
                break;
            }
        }
    });

    let mut pending = Some(first_pdu);
    while running.load(Ordering::SeqCst) {
        let pdu = if let Some(p) = pending.take() {
            p
        } else {
            match pdu_rx.recv_timeout(Duration::from_secs(300)) {
                Ok(pdu) => pdu,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    log::info!("Connection from {} idle timeout (300s), closing", peer);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    log::info!(
                        "Connection from {} reader exited (state={}, full_feature={})",
                        peer,
                        session.state_name(),
                        session_entered
                    );
                    break;
                }
            }
        };

        if shutting_down.load(Ordering::SeqCst)
            && pdu.opcode == opcode::LOGIN_REQUEST
            && !session_entered
        {
            let data = SessionData::default();
            let reject_pdu =
                data.create_login_reject(pdu.itt, pdu::login_status::TARGET_ERROR, 0x01);
            let _ = write_pdu_digest(&mut write_stream, &reject_pdu, false, false);
            break;
        }

        if !session_entered && pdu.opcode == opcode::LOGIN_REQUEST {
            let current_sessions = active_sessions.load(Ordering::SeqCst);
            if current_sessions >= max_sessions as usize {
                log::warn!(
                    "Session limit reached ({}/{}) — rejecting {}",
                    current_sessions,
                    max_sessions,
                    peer
                );
                let data = SessionData::default();
                let reject_pdu =
                    data.create_login_reject(pdu.itt, pdu::login_status::TARGET_ERROR, 0x02);
                let _ = write_pdu_digest(&mut write_stream, &reject_pdu, false, false);
                break;
            }
        }

        let rx_level_info = !session.is_full_feature()
            || !matches!(
                pdu.opcode,
                opcode::SCSI_COMMAND | opcode::SCSI_DATA_OUT | opcode::NOP_OUT
            );
        if rx_level_info {
            log::info!(
                "RX {} from {} (opcode 0x{:02x}, state={}, data_len={})",
                pdu.opcode_name(),
                peer,
                pdu.opcode,
                session.state_name(),
                pdu.data.len()
            );
        } else {
            log::debug!(
                "RX {} from {} (opcode 0x{:02x}, state={}, data_len={})",
                pdu.opcode_name(),
                peer,
                pdu.opcode,
                session.state_name(),
                pdu.data.len()
            );
        }

        let was_full_feature = session.is_full_feature();

        let responses = if session.is_login_phase() {
            if pdu.opcode != opcode::LOGIN_REQUEST {
                log::warn!(
                    "Invalid opcode 0x{:02x} during login from {}",
                    pdu.opcode,
                    peer
                );
                if let Some(data) = session.data() {
                    vec![data.create_invalid_request_during_login_reject(pdu.itt)]
                } else {
                    break;
                }
            } else {
                let old = std::mem::replace(&mut session, AnySession::new());
                let (new_session, responses) = old.process_login(&pdu, target_name)?;
                session = new_session;
                responses
            }
        } else if session.is_full_feature() {
            match pdu.opcode {
                opcode::SCSI_COMMAND => {
                    handle_scsi_command_boxed(&mut session, &pdu, &device, target_name)?
                }
                opcode::SCSI_DATA_OUT => handle_scsi_data_out_boxed(&mut session, &pdu, &device)?,
                opcode::TEXT_REQUEST => {
                    handle_text_request_multi(&mut session, &pdu, target_name, portals)?
                }
                opcode::NOP_OUT => vec![session.process_nop_out(&pdu)?],
                opcode::LOGOUT_REQUEST => {
                    let old = std::mem::replace(&mut session, AnySession::new());
                    let (new_session, response) = old.process_logout(&pdu)?;
                    session = new_session;
                    vec![response]
                }
                opcode::TASK_MANAGEMENT_REQUEST => handle_task_management(&mut session, &pdu)?,
                _ => {
                    log::warn!("Unhandled opcode from {}: 0x{:02x}", peer, pdu.opcode);
                    vec![]
                }
            }
        } else {
            log::info!(
                "Session from {} ended (state={})",
                peer,
                session.state_name()
            );
            break;
        };

        let entering_full_feature = !was_full_feature && session.is_full_feature();

        for resp_pdu in &responses {
            let tx_info = !session.is_full_feature()
                || entering_full_feature
                || !matches!(
                    resp_pdu.opcode,
                    opcode::SCSI_DATA_IN | opcode::SCSI_RESPONSE | opcode::R2T | opcode::NOP_IN
                );
            if tx_info {
                log::info!(
                    "TX {} to {} (opcode 0x{:02x}, data_len={})",
                    resp_pdu.opcode_name(),
                    peer,
                    resp_pdu.opcode,
                    resp_pdu.data.len()
                );
            } else {
                log::debug!(
                    "TX {} to {} (opcode 0x{:02x}, data_len={})",
                    resp_pdu.opcode_name(),
                    peer,
                    resp_pdu.opcode,
                    resp_pdu.data.len()
                );
            }
            write_pdu_digest(
                &mut write_stream,
                resp_pdu,
                use_header_digest.load(Ordering::SeqCst),
                use_data_digest.load(Ordering::SeqCst),
            )?;
        }

        if entering_full_feature {
            if let Some(data) = session.data() {
                let hd = matches!(data.params.header_digest, crate::session::DigestType::CRC32C);
                let dd = matches!(data.params.data_digest, crate::session::DigestType::CRC32C);
                use_header_digest.store(hd, Ordering::SeqCst);
                use_data_digest.store(dd, Ordering::SeqCst);
                log::info!(
                    "Session from {} entered FullFeaturePhase: initiator={} HeaderDigest={} DataDigest={} MaxConnections={} MaxRecv={} MaxXmit={} MaxBurst={} FirstBurst={} ImmediateData={} InitialR2T={} portal={}",
                    peer,
                    data.params.initiator_name,
                    if hd { "CRC32C" } else { "None" },
                    if dd { "CRC32C" } else { "None" },
                    data.params.max_connections,
                    data.params.max_recv_data_segment_length,
                    data.params.max_xmit_data_segment_length,
                    data.params.max_burst_length,
                    data.params.first_burst_length,
                    data.params.immediate_data,
                    data.params.initial_r2t,
                    portal
                );
            }
            session_entered = true;
            active_sessions.fetch_add(1, Ordering::SeqCst);
            let initiator = session
                .data()
                .map(|d| d.params.initiator_name.clone())
                .unwrap_or_default();
            initiator_iqn = Some(initiator.clone());
            if let Some(ref sink) = session_events {
                sink.on_session_start(&SessionEvent {
                    target_iqn: target_name.to_string(),
                    initiator_iqn: initiator,
                    peer: peer.to_string(),
                });
            }
        }

        if session.is_ended() {
            log::info!(
                "Session from {} ending (state={})",
                peer,
                session.state_name()
            );
            break;
        }
    }

    conn_running.store(false, Ordering::SeqCst);
    let _ = write_stream.shutdown(Shutdown::Both);
    let _ = reader_handle.join();
    Ok((session_entered, initiator_iqn))
}

/// Handle a single PDU in multi-target mode (legacy helper retained for SCSI boxed path tests)
#[allow(dead_code)]
fn handle_pdu_multi_target(
    pdu: &IscsiPdu,
    session: &mut AnySession,
    device: &Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
    target_name: &str,
    target_address: &str,
    _stream: &mut TcpStream,
    _auth_config: &crate::auth::AuthConfig,
    _allowed_initiators: &Option<Vec<String>>,
    session_entered: &mut bool,
    active_sessions: &Arc<std::sync::atomic::AtomicUsize>,
) -> ScsiResult<Vec<IscsiPdu>> {
    match pdu.opcode {
        opcode::LOGIN_REQUEST => {
            let was_in_full_feature = session.is_full_feature();
            let old_session = std::mem::replace(session, AnySession::new());
            let (new_session, responses) = old_session.process_login(pdu, target_name)?;
            *session = new_session;
            if !was_in_full_feature && session.is_full_feature() {
                *session_entered = true;
                active_sessions.fetch_add(1, Ordering::SeqCst);
            }
            Ok(responses)
        }
        opcode::TEXT_REQUEST => handle_text_request(session, pdu, target_name, target_address),
        opcode::SCSI_COMMAND => handle_scsi_command_boxed(session, pdu, device, target_name),
        opcode::SCSI_DATA_OUT => handle_scsi_data_out_boxed(session, pdu, device),
        opcode::NOP_OUT => Ok(vec![session.process_nop_out(pdu)?]),
        opcode::LOGOUT_REQUEST => {
            let old_session = std::mem::replace(session, AnySession::new());
            let (new_session, response) = old_session.process_logout(pdu)?;
            *session = new_session;
            Ok(vec![response])
        }
        _ => {
            log::warn!("Unhandled opcode: 0x{:02x}", pdu.opcode);
            Ok(vec![])
        }
    }
}

/// Handle SCSI Command with boxed device
/// This is a wrapper around the generic handle_scsi_command() that works with trait objects
fn handle_scsi_command_boxed(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    device: &Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
    target_name: &str,
) -> ScsiResult<Vec<IscsiPdu>> {
    let cmd = pdu.parse_scsi_command()?;
    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("Session not in FullFeaturePhase".to_string()))?;

    log_scsi_command(&cmd, pdu.data.len());

    // Validate LUN
    if cmd.lun != 0 {
        log::warn!("Command 0x{:02x} ({}) to invalid LUN: 0x{:016x}", cmd.cdb[0], scsi_opcode_name(cmd.cdb[0]), cmd.lun);
        let sense = crate::scsi::SenseData::new(
            crate::scsi::sense_key::ILLEGAL_REQUEST,
            crate::scsi::asc::LOGICAL_UNIT_NOT_SUPPORTED,
            0,
        );
        return Ok(vec![IscsiPdu::scsi_response(
            cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
        )]);
    }

    // Validate CmdSN
    let cmd_sn = BigEndian::read_u32(&pdu.specific[4..8]);
    if !data.validate_cmd_sn(cmd_sn) {
        log::warn!("Invalid CmdSN: {}, expected: {}", cmd_sn, data.exp_cmd_sn);
    }

    let opcode = cmd.cdb[0];
    let is_sync_cache = opcode == 0x35 || opcode == 0x91;
    let is_write_cmd = matches!(opcode, 0x0a | 0x2a | 0x8a);

    // Handle WRITE commands
    if is_write_cmd {
        return handle_write_command_boxed(data, pdu, &cmd, device);
    }

    // Handle non-write commands
    let response = if opcode == 0x03 {
        // REQUEST SENSE
        log::info!("REQUEST SENSE called");
        if cmd.cdb.len() < 6 {
            ScsiResponse::check_condition(crate::scsi::SenseData::invalid_command())
        } else {
            let alloc_len = cmd.cdb[4] as usize;
            let mut sense_data = match &data.last_sense_data {
                Some(bytes) => bytes.clone(),
                None => crate::scsi::SenseData::new(
                    crate::scsi::sense_key::NO_SENSE,
                    crate::scsi::asc::NO_ADDITIONAL_SENSE,
                    0,
                ).to_bytes(),
            };
            sense_data.truncate(alloc_len.min(sense_data.len()));
            ScsiResponse::good(sense_data)
        }
    } else if is_sync_cache {
        let mut device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        device_guard.flush()?;
        ScsiResponse::good_no_data()
    } else {
        let device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        ScsiHandler::handle_command_with_target(&cmd.cdb, &**device_guard, None, Some(target_name))?
    };

    // Build response PDU(s)
    build_scsi_response(data, &cmd, response)
}

/// Handle write command with boxed device
fn handle_write_command_boxed(
    data: &mut SessionData,
    pdu: &IscsiPdu,
    cmd: &ScsiCommandPdu,
    device: &Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
) -> ScsiResult<Vec<IscsiPdu>> {
    let opcode = cmd.cdb[0];

    let (lba, transfer_length) = match opcode {
        0x0a | 0x2a => {
            if opcode == 0x0a && cmd.cdb.len() >= 6 {
                let lba_21 = ((cmd.cdb[1] as u32 & 0x1F) << 16)
                           | ((cmd.cdb[2] as u32) << 8)
                           | (cmd.cdb[3] as u32);
                (lba_21 as u64, cmd.cdb[4] as u32)
            } else if opcode == 0x2a && cmd.cdb.len() >= 10 {
                let lba = BigEndian::read_u32(&cmd.cdb[2..6]) as u64;
                let length = BigEndian::read_u16(&cmd.cdb[7..9]) as u32;
                (lba, length)
            } else {
                (0, 0)
            }
        }
        0x8a => {
            if cmd.cdb.len() >= 16 {
                let lba = BigEndian::read_u64(&cmd.cdb[2..10]);
                let length = BigEndian::read_u32(&cmd.cdb[10..14]);
                (lba, length)
            } else {
                (0, 0)
            }
        }
        _ => (0, 0),
    };

    if transfer_length > 0 {
        let device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        let block_size = device_guard.block_size();
        drop(device_guard);

        let expected_data_len = transfer_length as usize * block_size as usize;
        let bytes_received = pdu.data.len() as u32;

        // Check if this is a single-PDU write (all data fits in immediate data)
        if bytes_received as usize == expected_data_len {
            // Single-PDU write - write directly
            let mut device_guard = device.lock().map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
            if let Err(e) = device_guard.write(lba, &pdu.data, block_size) {
                log::error!("Write failed: {}", e);
                let sense = crate::scsi::SenseData::medium_error();
                return Ok(vec![IscsiPdu::scsi_response(
                    cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
                    pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
                )]);
            }
            return Ok(vec![IscsiPdu::scsi_response(
                cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
                pdu::scsi_status::GOOD, 0, 0, None,
            )]);
        }

        // Multi-PDU write - create buffer and copy immediate data
        let mut buffer = vec![0u8; expected_data_len];
        if !pdu.data.is_empty() {
            buffer[..pdu.data.len()].copy_from_slice(&pdu.data);
        }

        let ttt = data.next_target_transfer_tag();

        data.pending_writes.insert(cmd.itt, PendingWrite {
            lba, transfer_length, block_size, bytes_received, ttt, r2t_sn: 0, lun: cmd.lun,
            buffer,
            next_r2t_offset: bytes_received,
            expected_data_len: expected_data_len as u32,
            completed: false,
            r2t_pending: false,
        });

        // If F bit is clear, the initiator will send more unsolicited Data-Out
        // PDUs. Don't send R2T yet — the Data-Out handler will send R2T for
        // any remaining data after the unsolicited burst completes (F=true).
        if !cmd.final_flag {
            return Ok(vec![]);
        }

        // F bit set — no more unsolicited data. Send R2T for remainder.
        let max_burst = data.params.max_burst_length;
        let remaining = expected_data_len as u32 - bytes_received;
        let request_len = remaining.min(max_burst);

        if let Some(pending) = data.pending_writes.get_mut(&cmd.itt) {
            pending.next_r2t_offset = bytes_received + request_len;
            pending.r2t_sn = 1;
            pending.r2t_pending = true;
        }

        let r2t = IscsiPdu::r2t(
            cmd.lun, cmd.itt, ttt, data.stat_sn,
            data.exp_cmd_sn, data.max_cmd_sn,
            0, bytes_received, request_len,
        );

        return Ok(vec![r2t]);
    }

    Ok(vec![IscsiPdu::scsi_response(
        cmd.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
        pdu::scsi_status::GOOD, 0, 0, None,
    )])
}

/// Handle SCSI Data-Out with boxed device
fn handle_scsi_data_out_boxed(
    session: &mut AnySession,
    pdu: &IscsiPdu,
    device: &Arc<Mutex<Box<dyn ScsiBlockDevice + Send>>>,
) -> ScsiResult<Vec<IscsiPdu>> {
    let data_out = pdu.parse_scsi_data_out()?;
    let data = session.data_mut().ok_or_else(|| IscsiError::Protocol("Session not in FullFeaturePhase".to_string()))?;

    let pending = data.pending_writes.get_mut(&data_out.itt);
    if pending.is_none() {
        log::debug!("Data-Out for unknown ITT=0x{:08x}, ignoring", data_out.itt);
        return Ok(vec![]);
    }
    let pending = pending.unwrap();
    if pending.completed {
        log::debug!("Data-Out for completed ITT=0x{:08x}, absorbing", data_out.itt);
        if data_out.final_flag {
            data.pending_writes.remove(&data_out.itt);
        }
        return Ok(vec![]);
    }

    let block_size = pending.block_size;
    let transfer_length = pending.transfer_length;
    let lba = pending.lba;
    let total_expected = transfer_length * block_size;

    // Copy data into buffer at the correct offset (same rules as IscsiTarget path).
    let start_offset = data_out.buffer_offset as usize;
    let end_offset = start_offset + data_out.data.len();

    if end_offset > pending.buffer.len() {
        log::error!(
            "DATA-OUT offset {} + len {} exceeds buffer size {}",
            data_out.buffer_offset,
            data_out.data.len(),
            pending.buffer.len()
        );
        let sense = crate::scsi::SenseData::medium_error();
        return Ok(vec![IscsiPdu::scsi_response(
            data_out.itt, data.next_stat_sn(), data.exp_cmd_sn, data.max_cmd_sn,
            pdu::scsi_status::CHECK_CONDITION, 0, 0, Some(&sense.to_bytes()),
        )]);
    }

    pending.buffer[start_offset..end_offset].copy_from_slice(&data_out.data);
    if end_offset as u32 > pending.bytes_received {
        pending.bytes_received = end_offset as u32;
    }

    log::debug!(
        "Data-Out: ITT=0x{:08x} off={} len={} F={} recv={}/{} TTT=0x{:08x}",
        data_out.itt,
        start_offset,
        data_out.data.len(),
        data_out.final_flag,
        pending.bytes_received,
        total_expected,
        data_out.ttt
    );

    // Complete only on F=1 with full length (don't finish while unsolicited/R2T burst in flight).
    if data_out.final_flag && pending.bytes_received >= total_expected {
        let itt = data_out.itt;
        log::debug!(
            "Write complete: ITT=0x{:08x} bytes={}/{}",
            itt,
            pending.bytes_received,
            total_expected
        );
        let buffer = pending.buffer.clone();
        pending.completed = true;

        let mut device_guard = device
            .lock()
            .map_err(|_| IscsiError::Scsi("Device lock poisoned".to_string()))?;
        let write_result = device_guard.write(lba, &buffer, block_size);
        drop(device_guard);

        let (status, sense) = match write_result {
            Ok(()) => (scsi_status::GOOD, None),
            Err(e) => {
                log::error!("Write failed: {}", e);
                (
                    pdu::scsi_status::CHECK_CONDITION,
                    Some(crate::scsi::SenseData::medium_error().to_bytes()),
                )
            }
        };

        return Ok(vec![IscsiPdu::scsi_response(
            itt,
            data.next_stat_sn(),
            data.exp_cmd_sn,
            data.max_cmd_sn,
            status,
            0,
            0,
            sense.as_deref(),
        )]);
    }

    // F bit set — current burst done; clear outstanding R2T slot.
    if data_out.final_flag {
        pending.r2t_pending = false;
    }

    // Only issue the next R2T after F=1 and when MaxOutstandingR2T allows it.
    // Never send R2T on intermediate (F=0) Data-Out PDUs within a burst.
    if data_out.final_flag && !pending.r2t_pending && pending.bytes_received < total_expected {
        let max_burst = data.params.max_burst_length;
        let remaining = total_expected - pending.bytes_received;
        let request_len = remaining.min(max_burst);
        let current_offset = pending.bytes_received;
        let r2t_sn = pending.r2t_sn;
        let ttt = pending.ttt;
        let lun = pending.lun;
        let itt = data_out.itt;

        pending.next_r2t_offset += request_len;
        pending.r2t_sn += 1;
        pending.r2t_pending = true;

        let r2t = IscsiPdu::r2t(
            lun,
            itt,
            ttt,
            data.stat_sn,
            data.exp_cmd_sn,
            data.max_cmd_sn,
            r2t_sn,
            current_offset,
            request_len,
        );

        return Ok(vec![r2t]);
    }

    Ok(vec![])
}

/// Builder for configuring a multi-target iSCSI server
pub struct IscsiServerBuilder {
    bind_addr: Option<String>,
    portal_addrs: Vec<String>,
    targets: std::collections::HashMap<String, (Box<dyn ScsiBlockDevice + Send>, String, crate::auth::AuthConfig, Option<Vec<String>>)>,
    max_connections: Option<u32>,
    max_sessions: Option<u32>,
    session_events: Option<Arc<dyn SessionEventSink>>,
}

impl IscsiServerBuilder {
    fn new() -> Self {
        Self {
            bind_addr: None,
            portal_addrs: Vec::new(),
            targets: std::collections::HashMap::new(),
            max_connections: None,
            max_sessions: None,
            session_events: None,
        }
    }

    pub fn bind_addr(mut self, addr: &str) -> Self {
        self.bind_addr = Some(addr.to_string());
        self
    }

    /// Single portal for SendTargets (convenience; overwrites `portal_addrs`).
    pub fn advertise_addr(mut self, addr: &str) -> Self {
        self.portal_addrs = vec![addr.to_string()];
        self
    }

    /// All client-reachable portals for SendTargets (MPIO / multi-instance).
    pub fn portal_addrs<I, S>(mut self, addrs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.portal_addrs = addrs.into_iter().map(Into::into).collect();
        self
    }

    /// Optional sink notified when FullFeature sessions start/end (per target IQN).
    pub fn session_events(mut self, sink: Arc<dyn SessionEventSink>) -> Self {
        self.session_events = Some(sink);
        self
    }

    pub fn add_target(
        mut self,
        iqn: String,
        device: Box<dyn ScsiBlockDevice + Send>,
        alias: Option<String>,
    ) -> Self {
        let alias = alias.unwrap_or_else(|| iqn.clone());
        self.targets.insert(iqn, (device, alias, crate::auth::AuthConfig::None, None));
        self
    }

    pub fn add_target_with_auth(
        mut self,
        iqn: String,
        device: Box<dyn ScsiBlockDevice + Send>,
        alias: Option<String>,
        auth_config: crate::auth::AuthConfig,
        allowed_initiators: Option<Vec<String>>,
    ) -> Self {
        let alias = alias.unwrap_or_else(|| iqn.clone());
        self.targets.insert(iqn, (device, alias, auth_config, allowed_initiators));
        self
    }

    pub fn max_connections(mut self, max: u32) -> Self {
        self.max_connections = Some(max);
        self
    }

    pub fn max_sessions(mut self, max: u32) -> Self {
        self.max_sessions = Some(max);
        self
    }

    pub fn build(self) -> ScsiResult<IscsiServer> {
        if self.targets.is_empty() {
            return Err(IscsiError::Config("At least one target must be configured".to_string()));
        }

        let bind_addr = self.bind_addr.unwrap_or_else(|| format!("0.0.0.0:{}", ISCSI_PORT));

        // Convert to TargetInfo structs
        let mut targets_map = std::collections::HashMap::new();
        for (iqn, (device, alias, auth_config, allowed_initiators)) in self.targets {
            if !iqn.starts_with("iqn.") && !iqn.starts_with("eui.") && !iqn.starts_with("naa.") {
                return Err(IscsiError::Config(
                    format!("Invalid IQN format: {}", iqn)
                ));
            }

            targets_map.insert(iqn, TargetInfo {
                device: Arc::new(Mutex::new(device)),
                alias,
                auth_config,
                allowed_initiators,
            });
        }

        Ok(IscsiServer {
            bind_addr,
            portal_addrs: self.portal_addrs,
            targets: Arc::new(Mutex::new(targets_map)),
            running: Arc::new(AtomicBool::new(false)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            max_connections: self.max_connections.unwrap_or(16),
            active_connections: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            max_sessions: self.max_sessions.unwrap_or(256),
            active_sessions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            session_events: self.session_events,
        })
    }
}

// ============================================================================
// Unit Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the iSCSI digest value and wire byte order against tgt / open-iscsi.
    ///
    /// 0xE3069283 is the standard check value for CRC-32C (the CRC catalogue
    /// literally names this parameterisation "CRC-32/ISCSI"): the CRC of the
    /// ASCII string "123456789". tgt emits the raw u32 in native byte order,
    /// which on the LE hosts iSCSI runs on is little-endian — so the on-wire
    /// digest bytes must be the value little-endian.
    #[test]
    fn iscsi_digest_matches_tgt_wire_format() {
        assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(iscsi_digest(b"123456789"), [0x83, 0x92, 0x06, 0xE3]);
    }

    /// The header digest must cover the BHS *and* the AHS (RFC 3720 10.2.1;
    /// matches tgt's `crc32c(bhs); if (ahssize) crc32c(ahs)`). Verify that
    /// extending the covered bytes with an AHS changes the digest, i.e. the
    /// AHS is genuinely included rather than ignored.
    #[test]
    fn header_digest_covers_bhs_and_ahs() {
        let bhs = [0xABu8; BHS_SIZE];
        let mut bhs_plus_ahs = bhs.to_vec();
        bhs_plus_ahs.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // one 4-byte AHS

        let d_bhs_only = iscsi_digest(&bhs);
        let d_with_ahs = iscsi_digest(&bhs_plus_ahs);
        assert_ne!(
            d_bhs_only, d_with_ahs,
            "header digest must include the AHS, not just the BHS"
        );
    }

    struct MockDevice {
        capacity: u64,
        block_size: u32,
        data: Vec<u8>,
    }

    impl MockDevice {
        fn new(capacity: u64, block_size: u32) -> Self {
            let size = (capacity * block_size as u64) as usize;
            MockDevice { capacity, block_size, data: vec![0u8; size] }
        }
    }

    impl ScsiBlockDevice for MockDevice {
        fn read(&self, lba: u64, blocks: u32, block_size: u32) -> ScsiResult<Vec<u8>> {
            let offset = (lba * block_size as u64) as usize;
            let len = (blocks * block_size) as usize;
            if offset + len > self.data.len() {
                return Err(IscsiError::Scsi("Read out of bounds".into()));
            }
            Ok(self.data[offset..offset + len].to_vec())
        }

        fn write(&mut self, lba: u64, data: &[u8], block_size: u32) -> ScsiResult<()> {
            let offset = (lba * block_size as u64) as usize;
            if offset + data.len() > self.data.len() {
                return Err(IscsiError::Scsi("Write out of bounds".into()));
            }
            self.data[offset..offset + data.len()].copy_from_slice(data);
            Ok(())
        }

        fn capacity(&self) -> u64 { self.capacity }
        fn block_size(&self) -> u32 { self.block_size }
    }

    #[test]
    fn test_builder_default() {
        let device = MockDevice::new(1000, 512);
        let target = IscsiTarget::builder().build(device).unwrap();
        assert_eq!(target.bind_addr, "0.0.0.0:3260");
        assert!(target.target_name.starts_with("iqn."));
    }

    #[test]
    fn test_builder_custom() {
        let device = MockDevice::new(1000, 512);
        let target = IscsiTarget::builder()
            .bind_addr("127.0.0.1:3260")
            .target_name("iqn.2025-12.test:disk1")
            .target_alias("Test Disk")
            .build(device)
            .unwrap();

        assert_eq!(target.bind_addr, "127.0.0.1:3260");
        assert_eq!(target.target_name, "iqn.2025-12.test:disk1");
    }

    #[test]
    fn test_builder_invalid_iqn() {
        let device = MockDevice::new(1000, 512);
        let result = IscsiTarget::builder()
            .target_name("invalid-name")
            .build(device);
        assert!(result.is_err());
    }

    #[test]
    fn test_running_flag() {
        let device = MockDevice::new(1000, 512);
        let target = IscsiTarget::builder().build(device).unwrap();

        assert!(!target.is_running());
        target.running.store(true, Ordering::SeqCst);
        assert!(target.is_running());
        target.stop();
        assert!(!target.is_running());
    }
}
