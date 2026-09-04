//! Who may move what (design §6), in one place because two enforcers
//! read it.
//!
//! The operator renders this document from the `FlintRepo` CR into the
//! repository's state directory. `pre-receive` reads it and refuses a
//! push before the syncer ever sees it; the syncer reads the same
//! document and applies it again in step 2. The duplication is
//! deliberate and it is not belt-and-braces theatre: `receive-pack`
//! performs none of these checks itself under `receive.procReceiveRefs`
//! (§4), so if the hook were the only enforcer, a repository whose
//! `core.hooksPath` was wrong, whose hook binary was missing, or whose
//! image was rolled without it would silently accept a push to `main`
//! from anyone who could reach the door. One enforcer at the edge for a
//! clear message, one at the writer for the guarantee.
//!
//! **An absent policy is permissive**, the way lean's unstamped
//! `project_id` is: it is the pre-operator posture, and it is what the
//! rigs and the local spike run. A policy that refused everything when
//! it could not be found would make a rendering bug look exactly like a
//! locked repository.
//!
//! What this cannot express, stated rather than implied: `agentPattern`
//! restricts an agent to a shape of branch name, NOT to its own branch.
//! The principal a pod presents is its ServiceAccount, and many pods
//! share one; nothing in the token distinguishes `agent/pod-a` from
//! `agent/pod-b`. Per-pod branch ownership needs a per-pod principal,
//! which the door does not mint today.

use std::collections::BTreeMap;
use std::path::Path;

use super::gitcmd::is_zero;

/// Rendered by the operator beside the repository. Read by both hooks
/// and by the syncer.
pub const POLICY_FILE: &str = "policy.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Policy {
    /// Refs no push may move directly unless `pushers` names the
    /// principal, and that no push may DELETE at all.
    pub protected: Vec<String>,
    /// Ref pattern -> principals allowed to push it directly.
    /// Authoritative wherever it matches: a ref named here is governed
    /// by its list whether or not it is also protected.
    pub pushers: BTreeMap<String, Vec<String>>,
    /// Merge target -> principals allowed to push `refs/for/<target>`.
    /// A target with no entry is open when the target is unprotected
    /// (anyone who could push it directly may propose to it) and closed
    /// when it is protected (otherwise `refs/for` would be a way around
    /// the protection it exists to serve).
    pub merge_into: BTreeMap<String, Vec<String>>,
    /// The shape of ref an unlisted principal may create and push.
    /// Empty or absent: no shape restriction.
    pub agent_pattern: Option<String>,
    /// Refs a non-fast-forward push may move. Default: none.
    pub allow_non_fast_forward: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    /// The message the pusher sees, naming the rule.
    Refuse(String),
}

/// `*` matches any run of characters, `/` included — the same shape as
/// a refspec glob, which is what an operator writing `release/*` in the
/// CR expects. A pattern with no `*` is an exact ref name.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    if !name.starts_with(parts[0]) {
        return false;
    }
    let mut pos = parts[0].len();
    let last = parts.len() - 1;
    for (i, part) in parts.iter().enumerate().skip(1) {
        if i == last {
            return name.len() >= pos + part.len() && name[pos..].ends_with(part);
        }
        if part.is_empty() {
            continue;
        }
        match name[pos..].find(part) {
            Some(idx) => pos += idx + part.len(),
            None => return false,
        }
    }
    true
}

/// An operator writes `main` and `release/*` in the CR, not
/// `refs/heads/main`. Normalising here rather than at each comparison
/// means a policy written either way behaves the same, and it means a
/// pattern that already names `refs/tags/` still works.
pub fn full_ref(name: &str) -> String {
    if name.starts_with("refs/") {
        name.to_string()
    } else {
        format!("refs/heads/{name}")
    }
}

const FOR_PREFIX: &str = "refs/for/";

impl Policy {
    /// Load the rendered document. `Ok(None)` means there is none,
    /// which is permissive by design (see the module doc); an
    /// unparseable one is an error, because a policy the enforcers
    /// cannot read must never read as "no policy".
    pub fn load(dir: &Path) -> Result<Option<Policy>, String> {
        let path = dir.join(POLICY_FILE);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| format!("{} is unparseable: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn is_protected(&self, full: &str) -> bool {
        self.protected.iter().any(|p| glob_match(&full_ref(p), full))
    }

    pub fn allows_non_fast_forward(&self, full: &str) -> bool {
        self.allow_non_fast_forward.iter().any(|p| glob_match(&full_ref(p), full))
    }

    fn pushers_for(&self, full: &str) -> Option<&Vec<String>> {
        self.pushers
            .iter()
            .find(|(pattern, _)| glob_match(&full_ref(pattern), full))
            .map(|(_, who)| who)
    }

    fn mergers_for(&self, target_full: &str) -> Option<&Vec<String>> {
        self.merge_into
            .iter()
            .find(|(pattern, _)| glob_match(&full_ref(pattern), target_full))
            .map(|(_, who)| who)
    }

    /// Is this command allowed, for this principal?
    ///
    /// `principal` is `REMOTE_USER` as the door verified it. An EMPTY
    /// principal is a deployment with no door in front of it: the
    /// policy still applies, and every named list fails to contain it,
    /// so a protected ref stays protected even when nobody can be
    /// named. That is the safe direction, and it is why the check is
    /// membership rather than "is anyone named".
    pub fn judge(&self, principal: &str, ref_name: &str, new_oid: &str) -> Verdict {
        if let Some(target) = ref_name.strip_prefix(FOR_PREFIX) {
            return self.judge_merge(principal, &full_ref(target));
        }
        if !ref_name.starts_with("refs/") {
            return Verdict::Refuse(format!("{ref_name} is not under refs/"));
        }
        let deleting = is_zero(new_oid);
        if deleting && self.is_protected(ref_name) {
            // A protected ref can be MOVED by a listed pusher; nobody
            // deletes it through the door. Deleting `main` is not a
            // move, and there is no workflow that wants it by accident.
            return Verdict::Refuse(format!(
                "{ref_name} is protected and protected refs are never deleted through the server"
            ));
        }
        if let Some(who) = self.pushers_for(ref_name) {
            return if who.iter().any(|p| p == "*" || p == principal) {
                Verdict::Allow
            } else {
                Verdict::Refuse(format!(
                    "{ref_name} may be pushed only by {}",
                    render_list(who)
                ))
            };
        }
        if self.is_protected(ref_name) {
            let target = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);
            return Verdict::Refuse(format!(
                "{ref_name} is protected: push to refs/for/{target} to propose a merge"
            ));
        }
        if let Some(pattern) = self.agent_pattern.as_deref().filter(|p| !p.is_empty()) {
            if !glob_match(&full_ref(pattern), ref_name) {
                return Verdict::Refuse(format!(
                    "{principal} may push only refs matching {} in this repository",
                    full_ref(pattern)
                ));
            }
        }
        Verdict::Allow
    }

    fn judge_merge(&self, principal: &str, target_full: &str) -> Verdict {
        match self.mergers_for(target_full) {
            Some(who) if who.iter().any(|p| p == "*" || p == principal) => Verdict::Allow,
            Some(who) => Verdict::Refuse(format!(
                "only {} may propose merges into {target_full}",
                render_list(who)
            )),
            // No entry: open for a branch anyone could push directly,
            // closed for a protected one — otherwise refs/for would be
            // the way around the protection it exists to serve.
            None if self.is_protected(target_full) => Verdict::Refuse(format!(
                "{target_full} is protected and no principal is allowed to merge into it \
                 (set mergeInto in the FlintRepo)"
            )),
            None => Verdict::Allow,
        }
    }
}

fn render_list(who: &[String]) -> String {
    if who.is_empty() {
        "no one".to_string()
    } else {
        who.join(", ")
    }
}
