//! Local policy/storage adapters for the transfer ports. Fetch and push own disjoint files.
pub(crate) mod fetch;
mod push;

use crate::{
    ctx::Ctx,
    model::{Line, StateRec},
    repo::Repo,
    store_remote::Options,
};
use anyhow::{bail, Result};
use layr_store::sync::LocalReplica;

enum Access<'a, 'ctx> {
    Read(&'a Repo<'ctx>),
    Write(&'a mut Repo<'ctx>),
}

pub(crate) struct Local<'a, 'ctx> {
    access: Access<'a, 'ctx>,
    ctx: &'a Ctx,
    parent: Option<&'a Repo<'a>>,
    stage: Option<crate::repo::Temp>,
    bootstrap: bool,
}

impl<'a, 'ctx> Local<'a, 'ctx> {
    pub(crate) fn read(ctx: &'a Ctx, repo: &'a Repo<'ctx>) -> Self {
        Self { access: Access::Read(repo), ctx, parent: None, stage: None, bootstrap: false }
    }
    pub(crate) fn write(ctx: &'a Ctx, repo: &'a mut Repo<'ctx>, parent: Option<&'a Repo<'a>>) -> Self {
        Self { access: Access::Write(repo), ctx, parent, stage: None, bootstrap: false }
    }
    fn repo(&self) -> &Repo<'ctx> {
        match &self.access {
            Access::Read(repo) => repo,
            Access::Write(repo) => repo,
        }
    }
    fn repo_mut(&mut self) -> Result<&mut Repo<'ctx>> {
        match &mut self.access {
            Access::Write(repo) => Ok(repo),
            Access::Read(_) => bail!("a read-only replica cannot import records"),
        }
    }
}

impl LocalReplica for Local<'_, '_> {
    type State = StateRec;
    type Line = Line;
    type Selection = Options;

    fn visit_records(&self, machine: Option<(&str, u64)>, visit: &mut dyn FnMut(&str) -> Result<()>) -> Result<()> {
        if let Some((machine, after)) = machine {
            for line in self.repo().p.db.lines_after(machine, after)? {
                visit(&line)?;
            }
        } else {
            for line in self.repo().p.export_jsonl()?.lines() {
                visit(line)?;
            }
        }
        Ok(())
    }
    fn record_machines(&self) -> Result<Vec<String>> {
        self.repo().p.db.record_machines()
    }
    fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>> {
        self.repo().p.db.record_line(machine, seq)
    }
    fn verify_records(&self) -> Result<()> {
        self.repo().p.verify_records().map(|_| ())
    }
    fn import_records(
        &mut self,
        lines: &[String],
        keys: &std::collections::BTreeMap<String, ed25519_dalek::VerifyingKey>,
    ) -> Result<layr_store::sync::Imported> {
        self.import(lines, keys)
    }
    fn finish_import(&mut self, keys: &std::collections::BTreeMap<String, ed25519_dalek::VerifyingKey>) -> Result<()> {
        self.finish(keys)
    }
    fn fetch_selection(
        &mut self,
        records: &[String],
        selection: &Options,
        available: &[String],
        clone: bool,
    ) -> Result<layr_store::sync::FetchSelection<StateRec, Line>> {
        let lines = clone.then(|| fetch::source_lines(self.repo(), true)).transpose()?;
        self.select_fetch(records, selection, available, lines)
    }
    fn push_candidates(
        &self,
        records: &[String],
        selection: &Options,
        present: &std::collections::BTreeSet<String>,
        omitted: &mut Vec<String>,
    ) -> Result<Vec<StateRec>> {
        self.select_push(records, selection, present, omitted)
    }
    fn omitted_cache_refs(&self, opts: &Options) -> Result<Vec<String>> {
        fetch::omitted_cache_refs(self.repo(), opts)
    }
    fn tips(&self) -> Result<std::collections::BTreeSet<String>> {
        let repo = self.repo();
        let mut tips: std::collections::BTreeSet<_> = repo.p.db.lines()?.into_iter().map(|line| line.head).collect();
        tips.extend(repo.p.db.refs("", crate::ids::now_ms())?.into_iter().map(|reference| reference.target));
        Ok(tips)
    }
    fn ancestry(&self, id: &str) -> Result<Vec<String>> {
        fetch::state_ancestry(self.repo(), id)
    }
    fn pin_state(&self, id: &str) -> Result<()> {
        self.repo().hold(self.repo().pin(&[id])?);
        Ok(())
    }
    fn check_secrets(&self, ids: &[String]) -> Result<()> {
        crate::secrets::check_states(self.ctx, self.repo(), ids.iter().map(String::as_str))
    }
    fn has_state(&self, id: &str) -> bool {
        self.repo().p.has_state(id)
    }
    fn is_overlay(&self) -> bool {
        self.repo().p.is_overlay()
    }
    fn open_state(&self, id: &str) -> Result<Box<dyn layr_store::sync::StateRead + '_>> {
        self.open(id)
    }
    fn restore_base(&self, state: &StateRec, available: &std::collections::BTreeMap<String, String>) -> Option<String> {
        state
            .parents
            .iter()
            .chain(state.derived_from.iter())
            .find(|id| available.contains_key(*id) && self.has_state(id))
            .cloned()
    }
    fn stage_state(&mut self, parent: Option<&str>) -> Result<std::path::PathBuf> {
        self.stage(parent)
    }
    fn install_state(&mut self, state: &StateRec, transid: u64, files: usize) -> Result<usize> {
        self.install(state, transid, files)
    }
    fn abort_state(&mut self, error: anyhow::Error) -> anyhow::Error {
        match self.stage.take() {
            Some(stage) => stage.abort(error),
            None => error,
        }
    }
    fn git_has_store(&self) -> bool {
        crate::gitbridge::has_store(self.repo())
    }
    fn git_init(&self) -> Result<()> {
        crate::gitbridge::init_store(self.repo()).map(|_| ())
    }
    fn git_imported(&self) -> Result<Vec<String>> {
        self.repo().p.db.import_commits()
    }
    fn git_has_commits(&self, commits: &[String]) -> Result<Vec<bool>> {
        crate::gitbridge::has_commits(self.repo(), commits)
    }
    fn git_outside(&self, commits: &[String], covered: &[String]) -> Result<Vec<String>> {
        crate::gitbridge::commits_outside(self.repo(), commits, covered)
    }
    fn git_pack(&self, commits: &[String], covered: &[String]) -> Result<Vec<u8>> {
        crate::gitbridge::pack_commits(self.repo(), commits, covered)
    }
    fn git_install(&self, pack: &[u8]) -> Result<()> {
        crate::gitbridge::install_pack(self.repo(), pack)
    }
}
