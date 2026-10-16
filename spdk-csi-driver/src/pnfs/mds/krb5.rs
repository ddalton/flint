//! Kerberos on the MDS (pNFS Kerberos plan, Phase 1;
//! `docs/nfs-kerberos.md`, "pNFS").
//!
//! The MDS serves NFS through the same `serve_tcp` as the RWX server, so
//! the floor, allowlist, idmap and root squash need only their env. What
//! is MDS-specific is read here, once, at startup:
//!
//! - **The same refusals to start as the RWX server:** a bad
//!   `FLINT_NFS_MIN_SEC` or `FLINT_NFS_REQUIRE_MACH_CRED` stops the MDS,
//!   rather than leaving the floor on the ground.
//! - **No layouts for Kerberos callers.** Measured (plan, "Phase 0
//!   results", E1): a Linux client with a `sec=krb5i` mount that cannot
//!   get a GSS context with a data server sends that DS its I/O as
//!   AUTH_SYS, silently. Our DSes do no GSS yet, so a layout handed to a
//!   Kerberos caller would put its data on the wire unauthenticated and
//!   unsigned while the mount looks protected. With Kerberos on
//!   (`KRB5_KTNAME` set), LAYOUTGET from an RPCSEC_GSS call answers
//!   NFS4ERR_LAYOUTUNAVAIL, and every byte goes through the MDS under
//!   Kerberos. `FLINT_PNFS_LAYOUTS_FOR_GSS=true` turns layouts back on,
//!   for when the DSes do Kerberos (Phase 3). AUTH_SYS callers get
//!   layouts as before.

use crate::nfs::sec_policy::{SecLevel, SecPolicy};

/// `true` hands layouts to RPCSEC_GSS callers again (off by default;
/// meant for when the data servers do Kerberos). Unset or empty is off.
pub const LAYOUTS_FOR_GSS_ENV: &str = "FLINT_PNFS_LAYOUTS_FOR_GSS";

/// The MDS's security settings, validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MdsSecEnv {
    /// `FLINT_NFS_MIN_SEC`.
    pub floor: SecPolicy,
    /// `FLINT_NFS_REQUIRE_MACH_CRED`.
    pub require_mach_cred: bool,
    /// A keytab is configured (`KRB5_KTNAME`): GSS contexts can be made.
    pub kerberos: bool,
    /// LAYOUTGET from an RPCSEC_GSS call answers LAYOUTUNAVAIL.
    pub withhold_gss_layouts: bool,
}

impl MdsSecEnv {
    /// Validate already-read values (the seam tests use; `None` = unset).
    /// Any value that is not a recognised setting is an error, never "off".
    pub fn from_values(
        min_sec: Option<&str>,
        require_mach_cred: Option<&str>,
        krb5_ktname: Option<&str>,
        layouts_for_gss: Option<&str>,
    ) -> Result<Self, String> {
        let floor = SecPolicy::validate_env_value(min_sec.unwrap_or(""))?;
        let require_mach_cred =
            crate::nfs::v4::operations::session::parse_require_mach_cred(require_mach_cred)?;
        let kerberos = krb5_ktname.is_some_and(|k| !k.trim().is_empty());
        let layouts_for_gss = parse_layouts_for_gss(layouts_for_gss)?;
        Ok(Self {
            floor,
            require_mach_cred,
            kerberos,
            withhold_gss_layouts: kerberos && !layouts_for_gss,
        })
    }

    /// [`from_values`](Self::from_values) of this process's env.
    pub fn from_env() -> Result<Self, String> {
        let var = |k: &str| std::env::var(k).ok();
        Self::from_values(
            var(SecPolicy::ENV).as_deref(),
            var(crate::nfs::v4::operations::session::REQUIRE_MACH_CRED_ENV).as_deref(),
            var("KRB5_KTNAME").as_deref(),
            var(LAYOUTS_FOR_GSS_ENV).as_deref(),
        )
    }

    /// One line for the startup log.
    pub fn describe(&self) -> String {
        let floor = match self.floor.floor() {
            SecLevel::None => "none (every flavor accepted)".to_string(),
            level => format!("{} (weaker calls get AUTH_TOOWEAK)", level.name()),
        };
        format!(
            "floor {floor}; Kerberos {}; SP4_MACH_CRED {}; layouts for Kerberos callers {}",
            if self.kerberos {
                "on (KRB5_KTNAME)"
            } else {
                "off"
            },
            if self.require_mach_cred {
                "required"
            } else {
                "not required"
            },
            if self.withhold_gss_layouts {
                "WITHHELD (all their I/O through the MDS)"
            } else if self.kerberos {
                "granted (FLINT_PNFS_LAYOUTS_FOR_GSS)"
            } else {
                "n/a"
            },
        )
    }
}

/// [`LAYOUTS_FOR_GSS_ENV`]'s value: unset or empty is off; a typo is an
/// error, as `FLINT_NFS_REQUIRE_MACH_CRED`.
pub fn parse_layouts_for_gss(value: Option<&str>) -> Result<bool, String> {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("0") | Some("false") | Some("no") | Some("off") => Ok(false),
        Some("1") | Some("true") | Some("yes") | Some("on") => Ok(true),
        Some(other) => Err(format!(
            "{LAYOUTS_FOR_GSS_ENV}={other:?}: expected true or false"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_no_kerberos_and_layouts_for_everyone() {
        let e = MdsSecEnv::from_values(None, None, None, None).unwrap();
        assert_eq!(e.floor.floor(), SecLevel::None);
        assert!(!e.require_mach_cred);
        assert!(!e.kerberos);
        assert!(!e.withhold_gss_layouts);
    }

    #[test]
    fn kerberos_on_withholds_gss_layouts_unless_asked() {
        let on =
            MdsSecEnv::from_values(Some("krb5i"), Some("true"), Some("/k/keytab"), None).unwrap();
        assert_eq!(on.floor.floor(), SecLevel::Krb5i);
        assert!(on.require_mach_cred);
        assert!(on.kerberos);
        assert!(
            on.withhold_gss_layouts,
            "Phase 1 default: no layouts for GSS"
        );

        let knob =
            MdsSecEnv::from_values(Some("krb5i"), None, Some("/k/keytab"), Some("1")).unwrap();
        assert!(
            !knob.withhold_gss_layouts,
            "FLINT_PNFS_LAYOUTS_FOR_GSS=1 hands them out"
        );
        let off = MdsSecEnv::from_values(None, None, Some("/k/keytab"), Some("false")).unwrap();
        assert!(off.withhold_gss_layouts);
        // A blank KRB5_KTNAME is no keytab: no GSS context can be made.
        let blank = MdsSecEnv::from_values(None, None, Some("  "), None).unwrap();
        assert!(!blank.kerberos && !blank.withhold_gss_layouts);
    }

    #[test]
    fn bad_values_refuse_to_start() {
        let e = MdsSecEnv::from_values(Some("krb5pp"), None, None, None).unwrap_err();
        assert!(e.contains("krb5pp"), "{e}");
        assert!(
            MdsSecEnv::from_values(Some("krb5p"), None, None, None).is_err(),
            "krb5p is no floor"
        );
        let e = MdsSecEnv::from_values(None, Some("ture"), None, None).unwrap_err();
        assert!(e.contains("FLINT_NFS_REQUIRE_MACH_CRED"), "{e}");
        let e = MdsSecEnv::from_values(None, None, Some("/k"), Some("maybe")).unwrap_err();
        assert!(e.contains(LAYOUTS_FOR_GSS_ENV), "{e}");
    }

    #[test]
    fn describe_names_the_floor_and_the_layout_policy() {
        let on = MdsSecEnv::from_values(Some("krb5i"), Some("true"), Some("/k"), None).unwrap();
        let d = on.describe();
        assert!(
            d.contains("krb5i") && d.contains("WITHHELD") && d.contains("required"),
            "{d}"
        );
        let off = MdsSecEnv::from_values(None, None, None, None).unwrap();
        assert!(off.describe().contains("none"), "{}", off.describe());
    }
}
