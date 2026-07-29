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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StageState {
    #[serde(default)]
    pub current: Option<String>,
    #[serde(default)]
    pub releases: Vec<Release>,
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
    /// the newest `keep_releases`, never the serving or current one (INV-6).
    pub fn evictions(&self, keep_releases: u32) -> Vec<String> {
        let serving = self.serving().map(|r| r.container.clone());
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
            .collect()
    }

    /// Image tags safe to `docker image rm` after retention: app images of evicted
    /// releases plus managed-image versions beyond `keep_managed`, excluding any tag
    /// still referenced by a retained release (spec §7.13).
    pub fn images_to_gc(&self, keep_releases: u32, keep_managed: u32, managed_logicals: &[String]) -> Vec<String> {
        let evicted: HashSet<String> = self.evictions(keep_releases).into_iter().collect();
        let retained: Vec<&Release> = self.releases.iter().filter(|r| !evicted.contains(&r.container)).collect();
        let mut remove: Vec<String> = Vec::new();

        let retained_app: HashSet<&str> = retained.iter().filter_map(|r| r.app_image()).collect();
        for release in &self.releases {
            if evicted.contains(&release.container) {
                if let Some(app) = release.app_image() {
                    if !retained_app.contains(app) && !remove.iter().any(|t| t == app) {
                        remove.push(app.to_string());
                    }
                }
            }
        }

        for logical in managed_logicals {
            let retained_tags: HashSet<&str> =
                retained.iter().filter_map(|r| r.images.get(logical).map(String::as_str)).collect();
            for tag in self.managed_image_versions(logical).into_iter().skip(keep_managed as usize) {
                if !retained_tags.contains(tag.as_str()) && !remove.contains(&tag) {
                    remove.push(tag);
                }
            }
        }
        remove
    }

    /// Distinct tags ever recorded for a managed image, newest release first (GC input).
    pub fn managed_image_versions(&self, logical: &str) -> Vec<String> {
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
        assert_eq!(s.managed_image_versions("database"), vec!["db-b".to_string(), "db-a".to_string()]);
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
        let gc = s.images_to_gc(1, 2, &[]);
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
        let gc = s.images_to_gc(5, 1, &["database".to_string()]);
        // db-c retained (current). db-b referenced by retained c2. db-a referenced by retained c1.
        // with keep_releases=5 all releases retained -> managed tags all referenced -> nothing removed.
        assert!(gc.is_empty());
        // now drop retention so c1 is evicted -> db-a becomes removable
        let gc2 = s.images_to_gc(1, 1, &["database".to_string()]);
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
