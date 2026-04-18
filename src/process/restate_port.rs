//! Allocator for Restate port triples (ingress / admin / service).
//!
//! The supervisor stores allocations on the RunnerConfig itself, so the
//! "used" set is reconstructed on startup from config.runners.

use crate::config::RunnerConfig;
use std::collections::HashSet;

/// Inclusive range for Restate ingress ports.
pub const INGRESS_RANGE: std::ops::RangeInclusive<u16> = 18080..=18180;
/// Inclusive range for Restate admin ports.
pub const ADMIN_RANGE: std::ops::RangeInclusive<u16> = 19070..=19170;
/// Inclusive range for Restate service endpoint ports (the port the runner
/// hosts locally for Restate to call back into).
pub const SERVICE_RANGE: std::ops::RangeInclusive<u16> = 19080..=19180;

/// Collect the ports already claimed by existing runner configs.
fn claimed_ports(runners: &[RunnerConfig]) -> (HashSet<u16>, HashSet<u16>, HashSet<u16>) {
    let mut ingress = HashSet::new();
    let mut admin = HashSet::new();
    let mut service = HashSet::new();
    for r in runners {
        if let Some(p) = r.restate_ingress_port {
            ingress.insert(p);
        }
        if let Some(p) = r.restate_admin_port {
            admin.insert(p);
        }
        if let Some(p) = r.restate_service_port {
            service.insert(p);
        }
    }
    (ingress, admin, service)
}

/// Find the first free port in a range not present in the used set.
fn first_free(range: std::ops::RangeInclusive<u16>, used: &HashSet<u16>) -> Option<u16> {
    range.into_iter().find(|p| !used.contains(p))
}

/// Allocate a full Restate port triple (ingress, admin, service) that does not
/// collide with any port stored on an existing runner config.
///
/// MUST be called under the same write lock as the overall runner-port
/// conflict check so the reservation is race-free.
pub fn allocate_triple(runners: &[RunnerConfig]) -> Result<(u16, u16, u16), String> {
    let (ingress_used, admin_used, service_used) = claimed_ports(runners);

    let ingress = first_free(INGRESS_RANGE, &ingress_used).ok_or_else(|| {
        format!(
            "No free Restate ingress port in range {}..={}",
            INGRESS_RANGE.start(),
            INGRESS_RANGE.end()
        )
    })?;
    let admin = first_free(ADMIN_RANGE, &admin_used).ok_or_else(|| {
        format!(
            "No free Restate admin port in range {}..={}",
            ADMIN_RANGE.start(),
            ADMIN_RANGE.end()
        )
    })?;
    let service = first_free(SERVICE_RANGE, &service_used).ok_or_else(|| {
        format!(
            "No free Restate service port in range {}..={}",
            SERVICE_RANGE.start(),
            SERVICE_RANGE.end()
        )
    })?;

    Ok((ingress, admin, service))
}

/// Allocate only a service port. Used when the caller supplies explicit
/// `external_restate_admin_url` / `external_restate_ingress_url` — the
/// ingress/admin are hosted externally, but the runner still needs a local
/// service port for Restate to call back into.
pub fn allocate_service_only(runners: &[RunnerConfig]) -> Result<u16, String> {
    let (_, _, service_used) = claimed_ports(runners);
    first_free(SERVICE_RANGE, &service_used).ok_or_else(|| {
        format!(
            "No free Restate service port in range {}..={}",
            SERVICE_RANGE.start(),
            SERVICE_RANGE.end()
        )
    })
}

/// Resolved Restate port fields for a RunnerConfig, suitable for writing
/// directly into the struct fields. Centralises the branching over
/// (explicit triple) / (external URLs + service only) / (auto-allocate all).
pub struct ResolvedRestatePorts {
    pub ingress_port: Option<u16>,
    pub admin_port: Option<u16>,
    pub service_port: Option<u16>,
    pub external_admin_url: Option<String>,
    pub external_ingress_url: Option<String>,
}

/// Centralised resolver used by every spawn / register path. Caller must hold
/// the runners write lock and pass a snapshot of existing configs.
///
/// Branches:
/// - Any of the three explicit ports set: validate against existing runners, pass through as-is.
/// - Both external URLs set (and no explicit ports): allocate service port only.
/// - Otherwise: allocate full triple.
///
/// When `server_mode` is false this returns all `None` fields (no allocation).
pub fn resolve_ports(
    runners: &[RunnerConfig],
    server_mode: bool,
    explicit_ingress: Option<u16>,
    explicit_admin: Option<u16>,
    explicit_service: Option<u16>,
    external_admin_url: Option<String>,
    external_ingress_url: Option<String>,
) -> Result<ResolvedRestatePorts, String> {
    if !server_mode {
        return Ok(ResolvedRestatePorts {
            ingress_port: None,
            admin_port: None,
            service_port: None,
            external_admin_url: None,
            external_ingress_url: None,
        });
    }

    let has_explicit_any =
        explicit_ingress.is_some() || explicit_admin.is_some() || explicit_service.is_some();
    let has_external_pair = external_admin_url.is_some() && external_ingress_url.is_some();

    if has_explicit_any {
        validate_explicit_ports(runners, explicit_ingress, explicit_admin, explicit_service)?;
        return Ok(ResolvedRestatePorts {
            ingress_port: explicit_ingress,
            admin_port: explicit_admin,
            service_port: explicit_service,
            external_admin_url,
            external_ingress_url,
        });
    }

    if has_external_pair {
        let service = allocate_service_only(runners)?;
        return Ok(ResolvedRestatePorts {
            ingress_port: None,
            admin_port: None,
            service_port: Some(service),
            external_admin_url,
            external_ingress_url,
        });
    }

    let (ingress, admin, service) = allocate_triple(runners)?;
    Ok(ResolvedRestatePorts {
        ingress_port: Some(ingress),
        admin_port: Some(admin),
        service_port: Some(service),
        external_admin_url: None,
        external_ingress_url: None,
    })
}

/// Validate that the caller-supplied Restate ports do not collide with any
/// already-stored allocation. Returns a human-readable error on collision.
pub fn validate_explicit_ports(
    runners: &[RunnerConfig],
    ingress: Option<u16>,
    admin: Option<u16>,
    service: Option<u16>,
) -> Result<(), String> {
    let (ingress_used, admin_used, service_used) = claimed_ports(runners);
    if let Some(p) = ingress {
        if ingress_used.contains(&p) {
            return Err(format!(
                "Restate ingress port {} is already in use by another runner",
                p
            ));
        }
    }
    if let Some(p) = admin {
        if admin_used.contains(&p) {
            return Err(format!(
                "Restate admin port {} is already in use by another runner",
                p
            ));
        }
    }
    if let Some(p) = service {
        if service_used.contains(&p) {
            return Err(format!(
                "Restate service port {} is already in use by another runner",
                p
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RunnerConfig;

    fn cfg_with_ports(
        ingress: Option<u16>,
        admin: Option<u16>,
        service: Option<u16>,
    ) -> RunnerConfig {
        let mut c = RunnerConfig::default_primary();
        c.id = "t".to_string();
        c.is_primary = false;
        c.server_mode = true;
        c.restate_ingress_port = ingress;
        c.restate_admin_port = admin;
        c.restate_service_port = service;
        c
    }

    #[test]
    fn allocate_empty_returns_range_starts() {
        let (i, a, s) = allocate_triple(&[]).expect("first triple");
        assert_eq!(i, *INGRESS_RANGE.start());
        assert_eq!(a, *ADMIN_RANGE.start());
        assert_eq!(s, *SERVICE_RANGE.start());
    }

    #[test]
    fn allocate_skips_used_ports() {
        let used = vec![cfg_with_ports(
            Some(*INGRESS_RANGE.start()),
            Some(*ADMIN_RANGE.start()),
            Some(*SERVICE_RANGE.start()),
        )];
        let (i, a, s) = allocate_triple(&used).expect("next triple");
        assert_eq!(i, *INGRESS_RANGE.start() + 1);
        assert_eq!(a, *ADMIN_RANGE.start() + 1);
        assert_eq!(s, *SERVICE_RANGE.start() + 1);
    }

    #[test]
    fn service_only_skips_used() {
        let used = vec![cfg_with_ports(None, None, Some(*SERVICE_RANGE.start()))];
        let s = allocate_service_only(&used).expect("service port");
        assert_eq!(s, *SERVICE_RANGE.start() + 1);
    }

    #[test]
    fn validate_detects_collision() {
        let used = vec![cfg_with_ports(Some(18090), None, None)];
        assert!(validate_explicit_ports(&used, Some(18090), None, None).is_err());
        assert!(validate_explicit_ports(&used, Some(18091), None, None).is_ok());
    }
}
