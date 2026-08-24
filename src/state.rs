//! Persistent deploy state (`dcd-state.json`) and the §4.3 status transitions.
//! `serving` = the `cutover_pending` release if one exists, else `current`;
//! INV-10 keeps at most one `cutover_pending`. Pure logic, exhaustively traced.

use std::collections::HashSet;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    CutoverPending,
    Active,
    Superseded,
    RolledBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinalizeKind {
    Deploy,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    pub container: String,
    #[serde(default)]
    pub images: IndexMap<String, String>,
    pub created_at: u64,
    pub status: ReleaseStatus,
    #[serde(default)]
    pub ran_migrations: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub env_keys: Vec<String>,
}

impl Release {
    pub fn app_image(&self) -> Option<&str> {
        self.images.get("app").map(String::as_str)
    }
}

/// One tag dcd pulled onto this host, recorded before the pull itself so a deploy
/// that dies before `finalize` still leaves a reclaimable trace (spec §7.13).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PulledImage {
    pub logical: String,
    pub tag: String,
    pub pulled_at: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageState {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub releases: Vec<Release>,
    #[serde(default, deserialize_with = "de_lenient_seq")]
    pub pulled: Vec<PulledImage>,
}

/// An empty Lua table round-trips as an empty *map*, not a sequence — the same
/// ambiguity `config::de_lenient_map` handles from the other side. Without this a
/// plugin touching `ctx.state` at all would abort the deploy after cutover.
fn de_lenient_seq<'de, D>(deserializer: D) -> std::result::Result<Vec<PulledImage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct SeqOrEmptyMap;
    impl<'de> serde::de::Visitor<'de> for SeqOrEmptyMap {
        type Value = Vec<PulledImage>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a sequence (or an empty map)")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(entry) = access.next_element()? {
                out.push(entry);
            }
            Ok(out)
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut access: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((_, entry)) = access.next_entry::<serde::de::IgnoredAny, PulledImage>()? {
                out.push(entry);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_any(SeqOrEmptyMap)
}

/// How many versions of each image to keep, as the engine resolves it from
/// `retention`. Kept here (not in `config`) so state logic stays config-free.
#[derive(Debug, Clone, Default)]
pub struct KeepPolicy {
    pub releases: u32,
    pub managed_images: u32,
    pub per_logical: IndexMap<String, u32>,
}

impl KeepPolicy {
    pub fn images_of(&self, logical: &str) -> u32 {
        self.per_logical.get(logical).copied().unwrap_or(self.managed_images)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    #[serde(default)]
    pub stages: IndexMap<String, StageState>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: 1,
            stages: IndexMap::new(),
        }
    }
}

impl State {
    pub fn from_json(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("state serializes")
    }

    pub fn stage(&self, stage: &str) -> Option<&StageState> {
        self.stages.get(stage)
    }

    pub fn stage_mut(&mut self, stage: &str) -> &mut StageState {
        if !self.stages.contains_key(stage) {
            self.stages.insert(stage.to_string(), StageState::default());
        }
        self.stages.get_mut(stage).expect("just inserted")
    }

    /// Tags safe to `docker image rm` on `stage`'s behalf: what that stage's history
    /// and pull ledger propose, minus every tag another stage on this host still
    /// records. Stages share one repository, so a stage-local view would delete a
    /// sibling stage's rollback target (INV-6).
    pub fn images_to_gc(&self, stage: &str, keep: &KeepPolicy, logicals: &[String]) -> Vec<String> {
        let Some(state) = self.stage(stage) else {
            return Vec::new();
        };
        let elsewhere = self.tags_recorded_outside(stage);
        state
            .gc_candidates(keep, logicals)
            .into_iter()
            .filter(|tag| !elsewhere.contains(tag))
            .collect()
    }

    /// Every tag any other stage still names, in a release or in its pull ledger.
    pub fn tags_recorded_outside(&self, stage: &str) -> HashSet<String> {
        let mut recorded = HashSet::new();
        for (name, other) in &self.stages {
            if name == stage {
                continue;
            }
            for release in &other.releases {
                recorded.extend(release.images.values().cloned());
            }
            for entry in &other.pulled {
                recorded.insert(entry.tag.clone());
            }
        }
        recorded
    }

    /// Every tag any stage records — the ownership evidence the host-scoped sweep
    /// subtracts before proposing an unattributable tag for removal.
    pub fn all_recorded_tags(&self) -> HashSet<String> {
        let mut recorded = HashSet::new();
        for stage in self.stages.values() {
            for release in &stage.releases {
                recorded.extend(release.images.values().cloned());
            }
            for entry in &stage.pulled {
                recorded.insert(entry.tag.clone());
            }
        }
        recorded
    }
}

impl StageState {
    pub fn find(&self, container: &str) -> Option<&Release> {
        self.releases.iter().find(|r| r.container == container)
    }

    pub fn cutover_pending(&self) -> Option<&Release> {
        self.releases
            .iter()
            .find(|r| r.status == ReleaseStatus::CutoverPending)
    }

    /// The `cutover_pending` release `unlock` promotes. Identical to `cutover_pending()`
    /// under INV-10; the two differ only on corrupt state, where the most recently
    /// appended pending is the one that reached cutover last.
    pub fn newest_cutover_pending(&self) -> Option<&Release> {
        self.releases
            .iter()
            .rev()
            .find(|r| r.status == ReleaseStatus::CutoverPending)
    }

    pub fn pending_count(&self) -> usize {
        self.releases
            .iter()
            .filter(|r| r.status == ReleaseStatus::CutoverPending)
            .count()
    }

    /// The container currently receiving traffic: the cutover-pending release if
    /// one exists, else the finalized `current`.
    pub fn serving(&self) -> Option<&Release> {
        self.cutover_pending()
            .or_else(|| self.current.as_deref().and_then(|c| self.find(c)))
    }

    /// Append the new black as `cutover_pending` and demote any prior pending to
    /// `rolled_back` — one atomic step keeping exactly one pending (INV-10).
    pub fn record_cutover(&mut self, release: Release) {
        for existing in &mut self.releases {
            if existing.status == ReleaseStatus::CutoverPending {
                existing.status = ReleaseStatus::RolledBack;
            }
        }
        self.releases.push(release);
    }

    /// Apply the §4.3 finalize transition. `serving_before` is the container that
    /// was serving at run start (None on a first-ever deploy).
    pub fn finalize(&mut self, new_container: &str, kind: FinalizeKind, serving_before: Option<&str>) {
        let prev_current = self.current.clone();
        for release in &mut self.releases {
            if release.container == new_container {
                release.status = ReleaseStatus::Active;
            }
        }
        if let Some(before) = serving_before {
            if before != new_container {
                let demoted = match kind {
                    FinalizeKind::Rollback => ReleaseStatus::RolledBack,
                    FinalizeKind::Deploy => ReleaseStatus::Superseded,
                };
                self.set_status(before, demoted);
            }
        }
        if let Some(prev) = prev_current {
            if prev != new_container && Some(prev.as_str()) != serving_before {
                self.set_status(&prev, ReleaseStatus::Superseded);
            }
        }
        self.current = Some(new_container.to_string());
    }

    /// Restore INV-10 around the release `unlock` promotes: every other pending is
    /// demoted to `rolled_back`. Returns the demoted containers so the caller can
    /// report the corruption it just cleaned up.
    pub fn demote_other_pending_releases(&mut self, promoted: &str) -> Vec<String> {
        let mut demoted = Vec::new();
        for release in &mut self.releases {
            let is_stale_pending = release.status == ReleaseStatus::CutoverPending && release.container != promoted;
            if is_stale_pending {
                release.status = ReleaseStatus::RolledBack;
                demoted.push(release.container.clone());
            }
        }
        demoted
    }

    pub fn set_reason(&mut self, container: &str, reason: String) {
        if let Some(release) = self.releases.iter_mut().find(|r| r.container == container) {
            release.reason = Some(reason);
        }
    }

    fn set_status(&mut self, container: &str, status: ReleaseStatus) {
        if let Some(release) = self.releases.iter_mut().find(|r| r.container == container) {
            release.status = status;
        }
    }

    /// The release to roll back to: the most recent retained release before the
    /// serving one whose app image differs (skips a no-op rollback).
    pub fn rollback_target(&self) -> Option<&Release> {
        let serving = self.serving()?;
        self.releases
            .iter()
            .rev()
            .filter(|r| r.container != serving.container)
            .filter(|r| {
                matches!(
                    r.status,
                    ReleaseStatus::Active | ReleaseStatus::Superseded | ReleaseStatus::RolledBack
                )
            })
            .find(|r| r.app_image() != serving.app_image())
    }

    /// Containers to evict on retention: superseded/rolled-back releases beyond
    /// the newest `keep_releases`, never the serving or current one, and never the
    /// rollback target (INV-6).
    ///
    /// The rollback target is retained explicitly rather than assumed to fall inside
    /// `keep_releases`: `rollback_target` skips releases carrying the serving image
    /// (a no-op rollback), so after a same-tag redeploy the newest superseded release
    /// is not the target. At `keep_releases: 1` that lands the real target outside the
    /// keep window, and INV-6's guarantee — the target's image is never pruned — would
    /// break for exactly the deploys that reuse a tag.
    pub fn evictions(&self, keep_releases: u32) -> Vec<String> {
        let serving = self.serving().map(|r| r.container.clone());
        let rollback_container = self.rollback_target().map(|r| r.container.clone());
        let mut retained: Vec<&Release> = self
            .releases
            .iter()
            .filter(|r| matches!(r.status, ReleaseStatus::Superseded | ReleaseStatus::RolledBack))
            .collect();
        retained.sort_by_key(|r| std::cmp::Reverse(r.id));
        retained
            .into_iter()
            .skip(keep_releases as usize)
            .map(|r| r.container.clone())
            .filter(|c| Some(c) != serving.as_ref() && Some(c.clone()) != self.current)
            .filter(|c| Some(c) != rollback_container.as_ref())
            .collect()
    }

    /// Record a tag dcd is about to pull. Re-pulling a known tag refreshes its
    /// timestamp rather than duplicating it, so the ledger stays one row per tag.
    pub fn record_pull(&mut self, logical: &str, tag: &str, at: u64) {
        let known = self
            .pulled
            .iter_mut()
            .find(|entry| entry.logical == logical && entry.tag == tag);
        if let Some(entry) = known {
            entry.pulled_at = at;
            return;
        }
        self.pulled.push(PulledImage {
            logical: logical.to_string(),
            tag: tag.to_string(),
            pulled_at: at,
        });
    }

    /// Drop ledger rows for tags GC has decided are gone, so the ledger tracks the
    /// host rather than growing forever.
    pub fn forget_pulled(&mut self, removed: &[String]) {
        self.pulled.retain(|entry| !removed.contains(&entry.tag));
    }

    /// Image tags safe to `docker image rm` after retention, from this stage's point
    /// of view: app images of evicted releases, plus every logical's versions beyond
    /// its keep count, minus any tag a retained release still references (spec §7.13).
    /// The pull ledger is what makes a tag from a deploy that never finalized visible
    /// here at all.
    pub fn gc_candidates(&self, keep: &KeepPolicy, logicals: &[String]) -> Vec<String> {
        let evicted: HashSet<String> = self.evictions(keep.releases).into_iter().collect();
        let retained: Vec<&Release> = self.releases.iter().filter(|r| !evicted.contains(&r.container)).collect();
        let retained_tags: HashSet<&str> = retained.iter().flat_map(|r| r.images.values().map(String::as_str)).collect();
        let mut remove: Vec<String> = Vec::new();

        for release in &self.releases {
            let is_evicted = evicted.contains(&release.container);
            let Some(app) = release.app_image() else {
                continue;
            };
            if is_evicted && !retained_tags.contains(app) && !remove.iter().any(|t| t == app) {
                remove.push(app.to_string());
            }
        }

        for logical in logicals {
            let keep_count = keep.images_of(logical) as usize;
            for tag in self.image_versions(logical).into_iter().skip(keep_count) {
                if !retained_tags.contains(tag.as_str()) && !remove.contains(&tag) {
                    remove.push(tag);
                }
            }
        }
        remove
    }

    /// Distinct tags known for a logical, newest first: the pull ledger by pull time,
    /// then any tag only the release history knows (state written before the ledger
    /// existed, so older than everything pulled since).
    pub fn image_versions(&self, logical: &str) -> Vec<String> {
        // rows are appended in pull order, so a later row wins a `pulled_at` tie —
        // two pulls inside one clock second must not order newest-last
        let mut ledger: Vec<(usize, &PulledImage)> = self
            .pulled
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.logical == logical)
            .collect();
        ledger.sort_by_key(|(position, entry)| std::cmp::Reverse((entry.pulled_at, *position)));
        let ledger: Vec<&PulledImage> = ledger.into_iter().map(|(_, entry)| entry).collect();
        let mut versions: Vec<String> = Vec::new();
        for entry in ledger {
            if !versions.contains(&entry.tag) {
                versions.push(entry.tag.clone());
            }
        }
        for tag in self.recorded_image_versions(logical) {
            if !versions.contains(&tag) {
                versions.push(tag);
            }
        }
        versions
    }

    /// Distinct tags for a logical as the release history alone records them,
    /// newest release first.
    pub fn recorded_image_versions(&self, logical: &str) -> Vec<String> {
        let mut seen = Vec::new();
        for release in self.releases.iter().rev() {
            if let Some(tag) = release.images.get(logical) {
                if !seen.contains(tag) {
                    seen.push(tag.clone());
                }
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(id: u64, container: &str, app: &str, status: ReleaseStatus) -> Release {
        let mut images = IndexMap::new();
        images.insert("app".to_string(), app.to_string());
        Release {
            id,
            container: container.to_string(),
            images,
            created_at: id,
            status,
            ran_migrations: false,
            reason: None,
            env_keys: Vec::new(),
        }
    }

    fn keep(releases: u32, managed_images: u32) -> KeepPolicy {
        KeepPolicy {
            releases,
            managed_images,
            per_logical: IndexMap::new(),
        }
    }

    fn statuses(s: &StageState) -> Vec<(String, ReleaseStatus)> {
        s.releases.iter().map(|r| (r.container.clone(), r.status)).collect()
    }

    #[test]
    fn deploy_lifecycle_advances_current_and_supersedes() {
        let mut s = StageState::default();
        // (a) first deploy B1
        s.record_cutover(release(1, "B1", "img1", ReleaseStatus::CutoverPending));
        s.finalize("B1", FinalizeKind::Deploy, None);
        assert_eq!(s.current.as_deref(), Some("B1"));
        assert_eq!(s.find("B1").unwrap().status, ReleaseStatus::Active);

        // (b) deploy B2
        let serving_before = s.serving().map(|r| r.container.clone());
        s.record_cutover(release(2, "B2", "img2", ReleaseStatus::CutoverPending));
        s.finalize("B2", FinalizeKind::Deploy, serving_before.as_deref());
        assert_eq!(s.current.as_deref(), Some("B2"));
        assert_eq!(s.find("B1").unwrap().status, ReleaseStatus::Superseded);
        assert_eq!(s.find("B2").unwrap().status, ReleaseStatus::Active);
    }

    #[test]
    fn rollback_makes_fresh_black_current_not_the_target() {
        // state: B1 superseded (img1), B2 current/active (img2)
        let mut s = StageState::default();
        s.releases.push(release(1, "B1", "img1", ReleaseStatus::Superseded));
        s.releases.push(release(2, "B2", "img2", ReleaseStatus::Active));
        s.current = Some("B2".into());

        // (c) rollback -> target B1; fresh black R3 from B1 image
        let target = s.rollback_target().unwrap();
        assert_eq!(target.container, "B1");
        let serving_before = s.serving().map(|r| r.container.clone());
        s.record_cutover(release(3, "R3", "img1", ReleaseStatus::CutoverPending));
        s.finalize("R3", FinalizeKind::Rollback, serving_before.as_deref());

        assert_eq!(s.current.as_deref(), Some("R3"));
        assert_eq!(s.find("R3").unwrap().status, ReleaseStatus::Active);
        assert_eq!(s.find("B2").unwrap().status, ReleaseStatus::RolledBack);
        assert_eq!(s.find("B1").unwrap().status, ReleaseStatus::Superseded);
    }

    #[test]
    fn recovery_rollback_demotes_prior_pending_and_stale_current() {
        // crashed deploy: current=C_prev (active, container gone), P_crashed pending+serving
        let mut s = StageState::default();
        s.releases.push(release(1, "C_prev", "img1", ReleaseStatus::Active));
        s.releases.push(release(2, "P_crashed", "img2", ReleaseStatus::CutoverPending));
        s.current = Some("C_prev".into());
        assert_eq!(s.serving().unwrap().container, "P_crashed");

        // recovery rollback: target resolves before serving, fresh R3
        let serving_before = s.serving().map(|r| r.container.clone());
        s.record_cutover(release(3, "R3", "img1", ReleaseStatus::CutoverPending));
        // INV-10: exactly one pending after the atomic append
        assert_eq!(s.pending_count(), 1);
        assert_eq!(s.find("P_crashed").unwrap().status, ReleaseStatus::RolledBack);
        s.finalize("R3", FinalizeKind::Rollback, serving_before.as_deref());

        assert_eq!(s.current.as_deref(), Some("R3"));
        assert_eq!(s.find("R3").unwrap().status, ReleaseStatus::Active);
        assert_eq!(s.find("C_prev").unwrap().status, ReleaseStatus::Superseded);
        assert_eq!(s.find("P_crashed").unwrap().status, ReleaseStatus::RolledBack);
        // no phantom active, exactly one active == current
        let actives: Vec<_> = statuses(&s).into_iter().filter(|(_, st)| *st == ReleaseStatus::Active).collect();
        assert_eq!(actives, vec![("R3".to_string(), ReleaseStatus::Active)]);
    }

    #[test]
    fn unlock_promotes_the_newest_pending_and_supersedes_the_stale_current() {
        // a deploy that cut over and then died: current still points at the old red
        let mut s = StageState::default();
        s.releases.push(release(1, "C_prev", "img1", ReleaseStatus::Active));
        s.releases.push(release(2, "P_stuck", "img2", ReleaseStatus::CutoverPending));
        s.current = Some("C_prev".into());

        let promoted = s.newest_cutover_pending().unwrap().container.clone();
        assert_eq!(promoted, "P_stuck");
        let serving_before = s.current.clone();
        assert!(s.demote_other_pending_releases(&promoted).is_empty());
        s.finalize(&promoted, FinalizeKind::Deploy, serving_before.as_deref());

        assert_eq!(s.current.as_deref(), Some("P_stuck"));
        assert_eq!(s.find("P_stuck").unwrap().status, ReleaseStatus::Active);
        assert_eq!(s.find("C_prev").unwrap().status, ReleaseStatus::Superseded);
        assert_eq!(s.pending_count(), 0);
    }

    #[test]
    fn unlock_demotes_every_other_pending_on_corrupt_state() {
        let mut s = StageState::default();
        s.releases.push(release(1, "P_old", "img1", ReleaseStatus::CutoverPending));
        s.releases.push(release(2, "P_new", "img2", ReleaseStatus::CutoverPending));

        let promoted = s.newest_cutover_pending().unwrap().container.clone();
        assert_eq!(promoted, "P_new");
        assert_eq!(s.demote_other_pending_releases(&promoted), vec!["P_old".to_string()]);
        s.finalize(&promoted, FinalizeKind::Deploy, None);

        assert_eq!(s.pending_count(), 0);
        assert_eq!(s.find("P_old").unwrap().status, ReleaseStatus::RolledBack);
        assert_eq!(s.find("P_new").unwrap().status, ReleaseStatus::Active);
    }

    #[test]
    fn rollback_target_skips_noop_same_image() {
        let mut s = StageState::default();
        s.releases.push(release(1, "B1", "imgX", ReleaseStatus::Superseded));
        s.releases.push(release(2, "B2", "imgX", ReleaseStatus::Active)); // same image as B1
        s.current = Some("B2".into());
        // serving B2 image imgX; B1 is imgX too -> no-op -> no target
        assert!(s.rollback_target().is_none());
    }

    #[test]
    fn retention_keeps_current_and_newest_superseded_or_rolled_back() {
        let mut s = StageState::default();
        for id in 1..=5 {
            s.releases.push(release(id, &format!("S{id}"), &format!("img{id}"), ReleaseStatus::Superseded));
        }
        s.releases.push(release(6, "CUR", "img6", ReleaseStatus::Active));
        s.current = Some("CUR".into());
        // keep 3 -> evict the oldest 2 superseded (S1, S2)
        let mut evicted = s.evictions(3);
        evicted.sort();
        assert_eq!(evicted, vec!["S1".to_string(), "S2".to_string()]);
        // a rolled_back release is retained like superseded
        s.set_status("S3", ReleaseStatus::RolledBack);
        let mut evicted2 = s.evictions(3);
        evicted2.sort();
        assert_eq!(evicted2, vec!["S1".to_string(), "S2".to_string()]);
    }

    #[test]
    fn managed_image_versions_newest_first_distinct() {
        let mut s = StageState::default();
        let mut mk = |id: u64, db: &str| {
            let mut r = release(id, &format!("c{id}"), "app", ReleaseStatus::Superseded);
            r.images.insert("database".to_string(), db.to_string());
            s.releases.push(r);
        };
        mk(1, "db-a");
        mk(2, "db-a");
        mk(3, "db-b");
        assert_eq!(s.recorded_image_versions("database"), vec!["db-b".to_string(), "db-a".to_string()]);
    }

    /// INV-6 at the tightest setting: `rollback_target` skips releases carrying the
    /// serving image, so after a same-tag redeploy the real target sits outside the
    /// newest `keep_releases`. Retention must still spare it and its image.
    #[test]
    fn a_same_tag_redeploy_keeps_its_rollback_target_at_keep_one() {
        let mut s = StageState::default();
        s.releases.push(release(1, "c1", "imgA", ReleaseStatus::Superseded));
        s.releases.push(release(2, "c2", "imgB", ReleaseStatus::Superseded));
        s.releases.push(release(3, "c3", "imgB", ReleaseStatus::Active));
        s.current = Some("c3".into());

        assert_eq!(s.rollback_target().map(|r| r.container.as_str()), Some("c1"));
        assert!(!s.evictions(1).contains(&"c1".to_string()));
        assert!(!s.gc_candidates(&keep(1, 1), &[]).contains(&"imgA".to_string()));
    }

    #[test]
    fn images_to_gc_removes_evicted_unreferenced_only() {
        let mut s = StageState::default();
        // app images: S1=appA, S2=appA (shared), S3=appB, CUR=appC
        s.releases.push(release(1, "S1", "appA", ReleaseStatus::Superseded));
        s.releases.push(release(2, "S2", "appA", ReleaseStatus::Superseded));
        s.releases.push(release(3, "S3", "appB", ReleaseStatus::Superseded));
        s.releases.push(release(4, "CUR", "appC", ReleaseStatus::Active));
        s.current = Some("CUR".into());
        // keep 1 superseded -> evict S1, S2 (oldest). S3 retained.
        let gc = s.gc_candidates(&keep(1, 2), &[]);
        // appA evicted (S1,S2 both evicted, not referenced by retained) -> removed once
        assert_eq!(gc, vec!["appA".to_string()]);
        // appB still referenced by retained S3 -> NOT removed; appC is current -> NOT removed
        assert!(!gc.contains(&"appB".to_string()));
        assert!(!gc.contains(&"appC".to_string()));
    }

    #[test]
    fn images_to_gc_bounds_managed_versions() {
        let mut s = StageState::default();
        let mut mk = |id: u64, db: &str, status: ReleaseStatus| {
            let mut r = release(id, &format!("c{id}"), &format!("app{id}"), status);
            r.images.insert("database".to_string(), db.to_string());
            s.releases.push(r);
        };
        mk(1, "db-a", ReleaseStatus::Superseded);
        mk(2, "db-b", ReleaseStatus::Superseded);
        mk(3, "db-c", ReleaseStatus::Active);
        s.current = Some("c3".into());
        // keep_managed 1 -> only newest db tag (db-c) retained; db-a/db-b candidates,
        // but db-b's release c2 may be retained by keep_releases. Use keep_releases large so nothing app-evicts.
        let gc = s.gc_candidates(&keep(5, 1), &["database".to_string()]);
        // db-c retained (current). db-b referenced by retained c2. db-a referenced by retained c1.
        // with keep_releases=5 all releases retained -> managed tags all referenced -> nothing removed.
        assert!(gc.is_empty());
        // now drop retention so c1 is evicted -> db-a becomes removable
        let gc2 = s.gc_candidates(&keep(1, 1), &["database".to_string()]);
        assert!(gc2.contains(&"db-a".to_string()));
    }

    #[test]
    fn json_roundtrip() {
        let mut state = State::default();
        state
            .stage_mut("prod")
            .record_cutover(release(1, "c1", "img1", ReleaseStatus::Active));
        let json = state.to_json();
        let back = State::from_json(json.as_bytes()).unwrap();
        assert_eq!(back.stage("prod").unwrap().releases.len(), 1);
    }
}
