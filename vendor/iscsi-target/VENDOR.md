# Vendored `iscsi-target` 1.0.0

Upstream: https://github.com/lawless-m/iscsi-crate (crates.io `iscsi-target` 1.0.0).

Local changes:
- `IscsiTargetBuilder::advertise_addr` / `IscsiServerBuilder::advertise_addr` /
  `IscsiServerBuilder::portal_addrs` for SendTargets behind Docker/NAT and
  multi-portal (MPIO) discovery (`TargetAddress` repeated per portal).
- `ScsiBlockDevice::serial_number` / `naa_identifier` for VPD 0x80/0x83 so
  multipath can correlate the same LUN across portals/instances.
- Richer connection/login/SCSI logging (lifecycle at info; bulk R/W at debug).
- Login Response serializes ISID+TSIH (was zeroed on the wire).
- AuthMethod=None honours initiator Transit (CSG0→NSG1) instead of staying T=0.
- Negotiate MaxConnections; answer unknown operational keys with NotUnderstood.
- `IscsiServer` (shared portal, multi-IQN): digest-aware read/write after FullFeature,
  full discovery login before SendTargets, TargetAddress includes host:port.
- `IscsiServer` normal sessions use the same reader-thread + inline NOP path as
  `IscsiTarget` (keeps firmware keepalives alive during slow S3 I/O).
- StatSN: after final login into FullFeature, advance so the first SCSI response
  does not reuse the login StatSN.
- Shared-portal Data-Out path: do not send a new R2T on every non-final Data-Out
  (respect MaxOutstandingR2T=1); align with IscsiTarget write/R2T state machine.
- Optional `SessionEventSink` on `IscsiServer` for FullFeature session start/end
  (used by iscsi-s3 Prometheus metrics).
