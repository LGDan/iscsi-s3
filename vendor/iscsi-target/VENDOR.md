# Vendored `iscsi-target` 1.0.0

Upstream: https://github.com/lawless-m/iscsi-crate (crates.io `iscsi-target` 1.0.0).

Local changes:
- `IscsiTargetBuilder::advertise_addr` so SendTargets can return a reachable portal
  instead of the socket `local_addr` (important behind Docker/NAT).
- Richer connection/login/SCSI logging (lifecycle at info; bulk R/W at debug).
- Login Response serializes ISID+TSIH (was zeroed on the wire).
- AuthMethod=None honours initiator Transit (CSG0→NSG1) instead of staying T=0.
- Negotiate MaxConnections; answer unknown operational keys with NotUnderstood.
